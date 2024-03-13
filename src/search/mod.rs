use anyhow::Context;
use itertools::Itertools;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use tantivy::collector::Collector;
use tantivy::query::Query;
use tantivy::schema::{Field, Schema};
use tantivy::{DocAddress, Index};
use tokio::time::Interval;
use tracing::{debug, error, info};

use crate::nix::{self, NixPackage};
use crate::{Flake, LogError, NaiveNixosOption};

use self::options::OptionsSearcher;
use self::packages::PackagesSearcher;

type FCFruit = ((f32, f32), DocAddress);

pub mod options;
pub mod packages;

pub struct SearcherInner {
    schema: Schema,
    index: tantivy::Index,
    reader: tantivy::IndexReader,
    reference_field: Field,
}

struct ChannelSearcherInner {
    options: OptionsSearcher,
    packages: PackagesSearcher,
}

impl ChannelSearcherInner {
    /// attempt to load cached options
    #[tracing::instrument]
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

        let o_inner = OptionsSearcher::new_with_options(&options_index_path, options)
            .log_to_option("creating new options searcher")?;
        let p_inner = PackagesSearcher::new_with_packages(&package_index_path, packages)
            .log_to_option("creating new packages searcher")?;
        Some(Self {
            options: o_inner,
            packages: p_inner,
        })
    }
}

pub struct ChannelSearcher {
    inner: Option<ChannelSearcherInner>,

    // members required for updating the options at runtime
    branch_path: PathBuf,
    pub flake: Flake,
}

impl ChannelSearcher {
    pub fn active(&self) -> bool {
        self.inner.is_some()
    }

    pub fn search_options(&self, q: &str) -> Vec<&NaiveNixosOption> {
        self.inner
            .as_ref()
            .map(|i| i.options.search_entries(q))
            .unwrap_or_default()
    }

    pub fn search_packages(&self, q: &str) -> Vec<&NixPackage> {
        self.inner
            .as_ref()
            .map(|i| i.packages.search_entries(q))
            .unwrap_or_default()
    }

    pub fn start_timer(self, mut interval: Interval) -> Weak<Mutex<Self>> {
        info!("[{}] started timer", self.flake.branch);

        let searcher = Arc::new(Mutex::new(self));
        let ret = Arc::downgrade(&searcher);

        tokio::spawn(async move {
            loop {
                interval.tick().await;
                let (branch_path, f, active) = {
                    let s = searcher.lock().unwrap();
                    (s.branch_path.clone(), s.flake.clone(), s.active())
                };
                info!("[{}] starting update", f.branch);

                let latest_rev = Flake::get_latest_rev(&f.owner, &f.name, &f.branch).await;
                match latest_rev {
                    Ok(new_flake_rev) if !active || new_flake_rev != f.rev => {
                        if active {
                            info!("[{}] found newer revision: {:?}", f.branch, new_flake_rev);
                        } else {
                            info!(
                                "[{}] generating options for rev {:?}",
                                f.branch, new_flake_rev
                            );
                        }

                        match update_file_cache(&branch_path, &f) {
                            Ok((options, packages)) => {
                                info!("[{}] successfully updated branch", f.branch);

                                if !active {
                                    let inner = ChannelSearcherInner::new_with_values(
                                        &branch_path,
                                        options,
                                        packages,
                                    );

                                    let mut s = searcher.lock().unwrap();
                                    s.flake.rev = new_flake_rev;
                                    s.inner = inner;
                                } else {
                                    let mut s = searcher.lock().unwrap();
                                    // this is guaranteed to be true after the `active` check from above
                                    // but the type system insists on unpacking it
                                    // since this is not a critical path, unsafe unwrapping is not
                                    // warranted
                                    if let Some(ref mut i) = &mut s.inner {
                                        i.options
                                            .update_entries(options)
                                            .expect("could not update options");
                                        i.packages
                                            .update_entries(packages)
                                            .expect("could not update packages");
                                    } else {
                                        unreachable!(
                                            "[{}] channel searcher is active but inner is not some",
                                            f.branch
                                        );
                                    }
                                }
                            }
                            Err(e) => error!("[{}] error updating branch: {}", f.branch, e),
                        };
                    }
                    Ok(_) => info!("[{}] already up-to-date", f.branch),
                    Err(e) => error!("[{}] error getting the newest commit: {}", f.branch, e),
                };

                let period = interval.period();
                info!(
                    "[{}] next tick in {:?}h {:?}m",
                    f.branch,
                    period.as_secs() / (60 * 60),
                    (period.as_secs() / 60) % 60
                );
            }
        });
        ret
    }

    pub fn new(branch_path: &Path, flake: &Flake) -> Self {
        let inner = ChannelSearcherInner::maybe_load(branch_path);
        if inner.is_some() {
            debug!("[{}] loaded the channel from cache", flake.branch);
        } else {
            debug!("[{}] could not load the channel from cache", flake.branch);
        }
        Self {
            inner,
            branch_path: branch_path.to_path_buf(),
            flake: flake.clone(),
        }
    }
}

pub trait Searcher {
    type Item;

    fn load(&mut self, entries: HashMap<String, Self::Item>) -> anyhow::Result<()> {
        self.create_index()?;
        self.update_entries(entries)?;
        Ok(())
    }

    fn search_entries(&self, query: &str) -> Vec<&Self::Item>
    where
        Self::Item: std::fmt::Debug,
    {
        let Some(inner) = self.inner() else {
            error!("searcher not initialized yet, please call create_index first");
            return Vec::new();
        };

        let searcher = inner.reader.searcher();
        let results = {
            let query = self.parse_query(query);
            searcher.search(&query, &self.collector())
        };

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

                        self.entries()
                            .get(&name)
                            .expect("found option is not indexed")
                    })
                    .collect_vec()
            })
            .unwrap_or_default()
    }

    fn parse_query(&self, query_string: &str) -> Box<dyn Query>;
    fn create_index(&mut self) -> anyhow::Result<()>;
    fn update_entries(&mut self, entries: HashMap<String, Self::Item>) -> anyhow::Result<()>;
    fn entries(&self) -> &HashMap<String, Self::Item>;

    fn inner(&self) -> Option<&SearcherInner>;
    fn collector(&self) -> impl Collector<Fruit = Vec<FCFruit>>;
}

#[tracing::instrument(skip(branch_path))]
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

fn open_or_create_index(index_path: &Path, schema: &Schema) -> anyhow::Result<Index> {
    let mut index_tmp = Index::open_or_create(
        tantivy::directory::MmapDirectory::open(index_path).unwrap(),
        schema.clone(),
    )?;

    // recreate the schema if outdated
    if *schema != index_tmp.schema() {
        std::fs::remove_dir_all(index_path)?;
        index_tmp = Index::create_in_dir(index_path, schema.clone())?;
    }
    Ok(index_tmp)
}
