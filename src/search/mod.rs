use anyhow::Context;
use itertools::Itertools;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tantivy::collector::Collector;
use tantivy::query::Query;
use tantivy::schema::{Field, Schema};
use tantivy::{DocAddress, Index};
use tracing::{debug, error, info};

use crate::nix::{self, NixPackage};
use crate::{Flake, LogError, NaiveNixosOption};

type FCFruit = ((f32, f32), DocAddress);

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
    #[tracing::instrument(skip(branch_path, flake), fields(branch = flake.branch))]
    pub fn new(branch_path: &Path, flake: &Flake) -> Self {
        let inner = ChannelSearcherInner::maybe_load(branch_path);
        if inner.is_some() {
            debug!("loaded the channel from cache");
        } else {
            debug!("could not load the channel from cache");
        }

        Self {
            inner,
            branch_path: branch_path.to_path_buf(),
            flake: flake.clone(),
        }
    }

    pub fn active(&self) -> bool {
        self.inner.is_some()
    }

    pub fn search_options(&self, q: &str) -> Vec<NaiveNixosOption> {
        self.inner
            .as_ref()
            .map(|i| i.options.search_entries(q))
            .unwrap_or_default()
    }

    pub fn search_packages(&self, q: &str) -> Vec<NixPackage> {
        self.inner
            .as_ref()
            .map(|i| i.packages.search_entries(q))
            .unwrap_or_default()
    }

    #[tracing::instrument(skip(self), fields(branch = self.flake.branch))]
    pub async fn update(&mut self) -> anyhow::Result<()> {
        //anyhow::bail!("test error for logging");
        let active = self.active();
        let latest_rev =
            Flake::get_latest_rev(&self.flake.owner, &self.flake.name, &self.flake.branch).await;
        match latest_rev {
            Ok(new_flake_rev) if !active || new_flake_rev != self.flake.rev => {
                if active {
                    info!("current rev is {:?}", self.flake.rev);
                    info!("found newer revision: {:?}", new_flake_rev);
                } else {
                    info!("generating options for rev {:?}", new_flake_rev);
                }

                let mut new_flake = self.flake.clone();
                new_flake.rev = new_flake_rev;
                match update_file_cache(&self.branch_path, &new_flake) {
                    Ok((options, packages)) => {
                        info!("successfully updated file cache");

                        if !active {
                            let inner = ChannelSearcherInner::new_with_values(
                                &self.branch_path,
                                options,
                                packages,
                            );

                            self.flake = new_flake;
                            self.inner = inner;
                        } else {
                            // this is guaranteed to be true after the `active` check from above
                            // but the type system insists on unpacking it
                            // since this is not a critical path, unsafe unwrapping is not
                            // warranted
                            if let Some(ref mut i) = &mut self.inner {
                                i.options
                                    .update_entries(options)
                                    .context("could not update options")?;
                                i.packages
                                    .update_entries(packages)
                                    .context("could not update packages")?;
                            } else {
                                unreachable!("channel searcher is active but inner is not some");
                            }
                        }
                    }
                    Err(e) => error!("error updating branch: {}", e),
                };
            }
            Ok(_) => info!("already up-to-date"),
            Err(e) => error!("error getting the newest commit: {}", e),
        };

        Ok(())
    }
}

#[derive(Clone)]
pub struct GenericSearcher<Item> {
    pub index_path: PathBuf,
    pub map: HashMap<String, Item>,
    inner: Option<SearcherInner>,
}

impl<Item> GenericSearcher<Item> {
    pub fn new(index_path: &Path) -> Self {
        Self {
            index_path: index_path.to_path_buf(),
            map: HashMap::new(),
            inner: None,
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
        ret.create_index()?;
        ret.update_entries(entries)?;
        Ok(ret)
    }

    pub fn load(&mut self, entries: HashMap<String, Item>) -> anyhow::Result<()>
    where
        Self: Searcher<Item = Item>,
    {
        self.create_index()?;
        self.update_entries(entries)?;
        Ok(())
    }

    pub fn search_entries(&self, query: &str) -> Vec<Item>
    where
        Item: std::fmt::Debug + Clone,
        Self: Searcher,
    {
        let Some(ref inner) = self.inner else {
            error!("searcher not initialized yet, please call create_index first");
            return Vec::new();
        };

        let searcher = inner.reader.searcher();
        let query = self.parse_query(query);
        let results = searcher.search(&query, &self.collector());

        results
            .ok()
            .map(|top_docs| {
                top_docs
                    .into_iter()
                    .map(|(_score, doc_address)| {
                        let retrieved = searcher.doc(doc_address).unwrap();
                        let name = retrieved
                            .get_first(inner.reference_field)
                            .expect("result has a value for name")
                            .as_text()
                            .expect("value is text")
                            .to_string();

                        //dbg!((&name, &query.explain(&searcher, doc_address)));

                        let entry: Item = self
                            .map
                            .get(&name)
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

    // TODO these depend on the underlying generic type...
    // find a better way to implement this
    fn parse_query(&self, query_string: &str) -> Box<dyn Query>;
    fn create_index(&mut self) -> anyhow::Result<()>;
    fn update_entries(&mut self, entries: HashMap<String, Self::Item>) -> anyhow::Result<()>;
    fn collector(&self) -> impl Collector<Fruit = Vec<FCFruit>>;
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

#[tracing::instrument(skip(schema))]
fn open_or_create_index(index_path: &Path, schema: &Schema) -> anyhow::Result<Index> {
    let index_tmp = Index::open_or_create(
        tantivy::directory::MmapDirectory::open(index_path).unwrap(),
        schema.clone(),
    );

    match index_tmp {
        Ok(i) => Ok(i),
        Err(tantivy::TantivyError::SchemaError(e)) => {
            error!("schema error: {e}");
            debug!("deleting + recreating the old index");
            std::fs::remove_dir_all(index_path)?;
            std::fs::create_dir_all(index_path)?;
            Ok(Index::create_in_dir(index_path, schema.clone())?)
        }
        Err(e) => unreachable!("unexpected error: {e}"),
    }
}
