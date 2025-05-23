use anyhow::Context;
use itertools::Itertools;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tantivy::collector::{ScoreTweaker, TopDocs};
use tantivy::query::Query;
use tantivy::schema::{Field, OwnedValue, Schema};
use tantivy::tokenizer::{TextAnalyzer, WhitespaceTokenizer};
use tantivy::{Index, TantivyDocument};
use tracing::{debug, error, info};

use crate::nix::{self, NixPackage};
use crate::{Flake, FlakeRev, LogError, NaiveNixosOption};

pub mod options;
pub mod packages;

#[derive(Clone)]
pub struct SearcherInner {
    schema: Schema,
    index: tantivy::Index,
    reader: tantivy::IndexReader,
    reference_field: Field,
}

#[derive(Clone)]
struct ChannelSearcherInner {
    options: GenericSearcher<NaiveNixosOption>,
    packages: GenericSearcher<NixPackage>,
}

impl ChannelSearcherInner {
    /// attempt to load cached options
    pub fn maybe_load(branch_path: &Path) -> Option<Self> {
        let options = serde_json::from_str(
            &std::fs::read_to_string(branch_path.join("options.json"))
                .log_to_option("could not load options from disk")?,
        )
        .log_to_option("failed to deserialize options")?;

        let packages = serde_json::from_str(
            &std::fs::read_to_string(branch_path.join("packages.json"))
                .log_to_option("could not load package from cache")?,
        )
        .log_to_option("failed to deserialize packages json")?;

        Self::new_with_values(branch_path, options, packages)
    }

    pub fn new_with_values(
        branch_path: &Path,
        options: HashMap<String, NaiveNixosOption>,
        packages: HashMap<String, NixPackage>,
    ) -> Option<Self> {
        let options_index_path = branch_path.join("tantivy");
        let package_index_path = branch_path.join("tantivy_packages");

        let o_inner =
            GenericSearcher::<NaiveNixosOption>::new_with_values(&options_index_path, options)
                .log_to_option("creating new options searcher")?;
        let p_inner = GenericSearcher::<NixPackage>::new_with_values(&package_index_path, packages)
            .log_to_option("creating new packages searcher")?;
        Some(Self {
            options: o_inner,
            packages: p_inner,
        })
    }
}

#[derive(Clone)]
pub struct ChannelSearcher {
    inner: Option<ChannelSearcherInner>,

    // members required for updating the options at runtime
    branch_path: PathBuf,
    pub flake: Flake,
}

impl ChannelSearcher {
    #[tracing::instrument(skip(state_dir, flake), fields(branch = flake.branch))]
    pub fn in_statedir(state_dir: &Path, flake: &Flake) -> Self {
        let mut flake = flake.clone();
        let branchname = flake.branch.clone();
        let branch_path = state_dir.join(branchname.clone());

        info!("starting searcher for branch {}", &branchname);

        let flake_info_path = branch_path.join("flake_info.json");
        if matches!(flake.rev, FlakeRev::FallbackToCached) && flake_info_path.exists() {
            if let Ok(saved_flake) = serde_json::from_str::<Flake>(
                &std::fs::read_to_string(flake_info_path)
                    .expect("flake_info.json exists but could not be read"),
            ) {
                info!("loaded flake from file cache: {:#?}", saved_flake);
                flake = saved_flake;
            };
        }

        let inner = ChannelSearcherInner::maybe_load(&branch_path);
        if inner.is_some() {
            debug!("loaded the channel from cache");
        } else {
            debug!("could not load the channel from cache");
        }

        Self {
            inner,
            flake,
            branch_path: branch_path.to_path_buf(),
        }
    }

    pub fn active(&self) -> bool {
        self.inner.is_some()
    }

    pub fn search_options(&self, q: &str, n_items: u8, page: u8) -> Vec<NaiveNixosOption> {
        self.inner
            .as_ref()
            .map(|i| i.options.search_entries(q, n_items, page))
            .unwrap_or_default()
    }

    pub fn search_packages(&self, q: &str, n_items: u8, page: u8) -> Vec<NixPackage> {
        self.inner
            .as_ref()
            .map(|i| i.packages.search_entries(q, n_items, page))
            .unwrap_or_default()
    }

    #[tracing::instrument(skip(self), fields(branch = self.flake.branch))]
    pub async fn update(&mut self) -> anyhow::Result<()> {
        let active = self.active();
        let mut new_flake = self.flake.clone();

        match new_flake.get_newest_from_github().await {
            Ok(_) if active && new_flake.rev != self.flake.rev => {
                info!("current rev is {:?}", self.flake.rev);
                info!("found newer revision: {:?}", new_flake.rev);

                match update_file_cache(&self.branch_path, &new_flake) {
                    Ok((options, packages)) => {
                        info!("successfully updated file cache");
                        let Some(ref mut i) = &mut self.inner else {
                            unreachable!("channel searcher is active but inner is not some");
                        };

                        i.options
                            .update_entries(options)
                            .context("could not update options")?;
                        i.packages
                            .update_entries(packages)
                            .context("could not update packages")?;
                    }
                    Err(e) => error!("error updating branch: {}", e),
                }
            }

            Ok(_) if !active => match update_file_cache(&self.branch_path, &new_flake) {
                Ok((options, packages)) => {
                    info!("successfully updated file cache");
                    let inner =
                        ChannelSearcherInner::new_with_values(&self.branch_path, options, packages);

                    self.flake = new_flake;
                    self.inner = inner;
                }
                Err(e) => error!("error updating branch: {}", e),
            },
            Ok(_) => info!("already up-to-date"),
            Err(e) => error!("error getting the newest commit: {}", e),
        }

        Ok(())
    }
}

#[derive(Clone)]
pub struct GenericSearcher<Item> {
    pub index_path: PathBuf,
    pub map: HashMap<String, Item>,
    inner: SearcherInner,
}

impl<Item> GenericSearcher<Item>
where
    Self: Searcher,
{
    pub fn new(index_path: &Path) -> Self {
        let (attribute_name, schema) = Self::schema();
        let index = {
            std::fs::create_dir_all(index_path).unwrap();
            let index_tmp = Index::open_or_create(
                tantivy::directory::MmapDirectory::open(index_path).unwrap(),
                schema.clone(),
            );

            match index_tmp {
                Ok(i) => i,
                Err(tantivy::TantivyError::SchemaError(e)) => {
                    error!("schema error: {e}");
                    debug!("deleting + recreating the old index");
                    std::fs::remove_dir_all(index_path).unwrap();
                    std::fs::create_dir_all(index_path).unwrap();
                    Index::create_in_dir(index_path, schema.clone()).unwrap()
                }
                Err(e) => unreachable!("unexpected error: {e}"),
            }
        };

        let options_tk = TextAnalyzer::builder(WhitespaceTokenizer::default()).build();
        index.tokenizers().register("default_ws", options_tk);

        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::Manual)
            .try_into()
            .unwrap();

        let map = HashMap::new();
        let inner = SearcherInner {
            schema,
            index,
            reader,
            reference_field: attribute_name,
        };

        Self {
            index_path: index_path.to_path_buf(),
            map,
            inner,
        }
    }

    pub fn new_with_values(
        index_path: &Path,
        entries: HashMap<String, Item>,
    ) -> anyhow::Result<Self>
    where
        Self: Searcher<Item = Item>,
    {
        let mut ret = Self::new(index_path);
        ret.update_entries(entries)?;
        Ok(ret)
    }

    pub fn search_entries(&self, query: &str, n_items: u8, page: u8) -> Vec<Item>
    where
        Item: std::fmt::Debug + Clone,
        Self: Searcher,
    {
        let searcher = self.inner.reader.searcher();
        let query = self.parse_query(query);

        let collector = TopDocs::with_limit(n_items as usize + 1)
            .and_offset((page.max(1) - 1) as usize * n_items as usize)
            .tweak_score(Self::scorer());

        let results = searcher.search(&query, &collector);

        results
            .ok()
            .map(|top_docs| {
                top_docs
                    .into_iter()
                    .map(|(_score, doc_address)| {
                        let retrieved: TantivyDocument = searcher.doc(doc_address).unwrap();
                        let OwnedValue::Str(name) = retrieved
                            .get_first(self.inner.reference_field)
                            .expect("result has a value for name")
                        else {
                            unreachable!("can't be non-str");
                        };

                        let entry: Item = self
                            .map
                            .get(name)
                            .expect("found option is not indexed")
                            .clone();
                        entry
                    })
                    .collect_vec()
            })
            .unwrap_or_default()
    }
}

pub trait Searcher {
    type Item;

    fn parse_query(&self, query_string: &str) -> Box<dyn Query>;
    fn update_entries(&mut self, entries: HashMap<String, Self::Item>) -> anyhow::Result<()>;
    fn schema() -> (tantivy::schema::Field, tantivy::schema::Schema);
    fn scorer() -> impl ScoreTweaker<(f32, f32)> + Send;
}

pub fn update_file_cache(
    branch_path: &Path,
    flake: &Flake,
) -> anyhow::Result<(
    HashMap<String, NaiveNixosOption>,
    HashMap<String, NixPackage>,
)> {
    let options_index_path = branch_path.join("tantivy");
    let pkgs_index_path = branch_path.join("tantivy_packages");

    std::fs::create_dir_all(options_index_path.clone())
        .context("failed to create options index path")?;
    std::fs::create_dir_all(pkgs_index_path.clone())
        .context("failed to create packages index path")?;

    let (options, packages) = nix::build_options_for_fcio_branch(flake)?;
    std::fs::write(
        branch_path.join("options.json"),
        serde_json::to_string(&options).expect("failed to serialize naive options"),
    )
    .expect("failed to save naive options");
    std::fs::write(
        branch_path.join("packages.json"),
        serde_json::to_string(&packages).expect("failed to serialize packages"),
    )
    .expect("failed to save packages");

    // cache the current branch + revision
    std::fs::write(
        branch_path.join("flake_info.json"),
        serde_json::to_string(&flake).expect("failed to serialize flake info"),
    )
    .expect("failed to save flake info");

    info!("successfully rebuilt options, packages + index");
    Ok((options, packages))
}
