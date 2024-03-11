use anyhow::Context;
use itertools::Itertools;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use tantivy::collector::{Collector, TopDocs};
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, TextFieldIndexing, TextOptions, TEXT};
use tantivy::{DocAddress, DocId, Document, Index, Score, SegmentReader};
use tokio::time::Interval;
use tracing::{debug, error, info};

use crate::nix::{self, NixPackage};
use crate::{Flake, NaiveNixosOption};

#[allow(dead_code)]
struct SearcherInner {
    schema: Schema,
    index: tantivy::Index,
    query_parser: QueryParser,
    reader: tantivy::IndexReader,
    reference_field: Field,
}

pub trait Searcher {
    type Item;

    fn load(&mut self, entries: HashMap<String, Self::Item>) -> anyhow::Result<()> {
        self.create_index()?;
        self.update_entries(entries)?;
        Ok(())
    }

    fn create_index(&mut self) -> anyhow::Result<()>;
    fn update_entries(&mut self, entries: HashMap<String, Self::Item>) -> anyhow::Result<()>;
    fn search_entries(&self, query: &str) -> Vec<&Self::Item>;
    fn entries(&self) -> &HashMap<String, Self::Item>;
}

pub struct OptionsSearcher {
    pub index_path: PathBuf,
    pub options: HashMap<String, NaiveNixosOption>,
    inner: Option<SearcherInner>,
}

impl OptionsSearcher {
    pub fn new(index_path: &Path) -> Self {
        Self {
            index_path: index_path.to_path_buf(),
            options: HashMap::new(),
            inner: None,
        }
    }

    pub fn new_with_options(
        index_path: &Path,
        options: HashMap<String, NaiveNixosOption>,
    ) -> anyhow::Result<Self> {
        let mut ret = Self::new(index_path);
        ret.create_index()?;
        ret.update_entries(options)?;
        Ok(ret)
    }
}

impl Searcher for OptionsSearcher {
    type Item = NaiveNixosOption;

    fn entries(&self) -> &HashMap<String, Self::Item> {
        &self.options
    }

    /// creates the index and initializes the struct that holds
    /// fields that are important for searching
    fn create_index(&mut self) -> anyhow::Result<()> {
        let mut schema_builder = Schema::builder();

        let raw_stored = TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("raw")
                    .set_index_option(tantivy::schema::IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored();

        let raw_unstored = TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("raw")
                .set_index_option(tantivy::schema::IndexRecordOption::WithFreqsAndPositions),
        );

        let original_name = schema_builder.add_text_field("original_name", raw_stored);
        let name = schema_builder.add_text_field("name", TEXT);
        let description = schema_builder.add_text_field("description", TEXT);
        schema_builder.add_text_field("default", raw_unstored);
        let schema = schema_builder.build();

        let index = Index::open_or_create(
            tantivy::directory::MmapDirectory::open(&self.index_path).unwrap(),
            schema.clone(),
        )?;

        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::OnCommit)
            .try_into()
            .unwrap();

        let mut query_parser = QueryParser::for_index(&index, vec![name, description]);
        query_parser.set_field_fuzzy(name, true, 1, true);
        query_parser.set_field_boost(name, 5.0);
        query_parser.set_field_boost(description, 0.01);

        self.inner = Some(SearcherInner {
            schema,
            query_parser,
            index,
            reader,
            reference_field: original_name,
        });

        Ok(())
    }

    /// updates indexed + cached entries with new ones
    fn update_entries(&mut self, entries: HashMap<String, Self::Item>) -> anyhow::Result<()> {
        let Some(ref inner) = self.inner else {
            anyhow::bail!("can not update options before index creation");
        };

        let index = &inner.index;
        let schema = &inner.schema;

        let mut index_writer = index.writer(50_000_000)?;
        let name = schema
            .get_field("name")
            .expect("the field name should exist");
        let original_name = schema
            .get_field("original_name")
            .expect("the field original_name should exist");
        let description = schema
            .get_field("description")
            .expect("the description field should exist");
        let default = schema
            .get_field("default")
            .expect("the field default should exist");

        index_writer
            .delete_all_documents()
            .expect("failed to delete all documents");

        for (option_name, option) in &entries {
            let mut document = Document::default();
            document.add_text(original_name, option_name.clone());
            document.add_text(name, option_name.clone().replace('.', " "));
            document.add_text(description, option.description.0.clone());
            document.add_text(default, option.default.0.clone());
            index_writer.add_document(document)?;
        }

        index_writer.commit()?;
        self.options = entries;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    fn search_entries(&self, query: &str) -> Vec<&Self::Item> {
        let Some(ref inner) = self.inner else {
            panic!("searcher is not fully initialized, create the index first");
        };

        let searcher = inner.reader.searcher();
        let query = inner.query_parser.parse_query_lenient(query).0;
        debug!("{:#?}", query);

        let scorer = TopDocs::with_limit(10).tweak_score(move |segment_reader: &SegmentReader| {
            let store_reader = segment_reader.get_store_reader(100).unwrap();

            move |doc: DocId, mut score: Score| {
                let d = store_reader.get(doc).unwrap();
                let name = d.field_values().first().unwrap().value.as_text().unwrap();

                let fcio_option = name.starts_with("flyingcircus");
                let enable_option = name.ends_with("enable");

                if fcio_option {
                    score *= 1.05;
                }
                if enable_option {
                    score *= 1.05;
                }

                score
            }
        });

        let results = searcher.search(&query, &scorer);

        results
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

                        self.options
                            .get(&name)
                            .expect("found option is not indexed")
                    })
                    .collect_vec()
            })
            .unwrap_or_default()
    }
}

pub struct PackagesSearcher {
    pub index_path: PathBuf,
    pub packages: HashMap<String, NixPackage>,
    inner: Option<SearcherInner>,
}

impl PackagesSearcher {
    pub fn new(index_path: &Path) -> Self {
        Self {
            index_path: index_path.to_path_buf(),
            packages: HashMap::new(),
            inner: None,
        }
    }

    pub fn new_with_packages(
        index_path: &Path,
        packages: HashMap<String, NixPackage>,
    ) -> anyhow::Result<Self> {
        let mut ret = Self::new(index_path);
        ret.create_index()?;
        ret.update_entries(packages)?;
        Ok(ret)
    }
}

impl Searcher for PackagesSearcher {
    type Item = NixPackage;

    fn entries(&self) -> &HashMap<String, Self::Item> {
        &self.packages
    }

    fn create_index(&mut self) -> anyhow::Result<()> {
        let mut schema_builder = Schema::builder();

        let raw_stored = TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("raw")
                    .set_index_option(tantivy::schema::IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored();

        let attribute_name = schema_builder.add_text_field("attribute_name", raw_stored);
        let name = schema_builder.add_text_field("name", TEXT);
        let description = schema_builder.add_text_field("description", TEXT);
        schema_builder.add_text_field("long_description", TEXT);
        let schema = schema_builder.build();

        let index = Index::open_or_create(
            tantivy::directory::MmapDirectory::open(&self.index_path).unwrap(),
            schema.clone(),
        )?;

        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::OnCommit)
            .try_into()
            .unwrap();

        let mut query_parser = QueryParser::for_index(&index, vec![name, description]);
        query_parser.set_field_fuzzy(name, true, 1, true);
        query_parser.set_field_boost(name, 5.0);

        self.inner = Some(SearcherInner {
            schema,
            query_parser,
            index,
            reader,
            reference_field: attribute_name,
        });

        Ok(())
    }

    fn update_entries(&mut self, entries: HashMap<String, Self::Item>) -> anyhow::Result<()> {
        let Some(ref inner) = self.inner else {
            anyhow::bail!("can not update options before index creation");
        };

        let index = &inner.index;
        let schema = &inner.schema;
        let mut index_writer = index.writer(50_000_000)?;

        let attribute_name = schema
            .get_field("attribute_name")
            .expect("the field attribute_name should exist");
        let name = schema
            .get_field("name")
            .expect("the field name should exist");
        let description = schema
            .get_field("description")
            .expect("the field description should exist");
        let long_description = schema
            .get_field("long_description")
            .expect("the field long_description should exist");

        index_writer
            .delete_all_documents()
            .expect("failed to delete all documents");
        for (aname, package) in &entries {
            let mut document = Document::default();
            document.add_text(attribute_name, aname.clone());
            document.add_text(name, package.name.clone());
            document.add_text(description, package.description.clone().unwrap_or_default());
            document.add_text(
                long_description,
                package.long_description.clone().unwrap_or_default(),
            );
            index_writer.add_document(document)?;
        }

        index_writer.commit()?;
        self.packages = entries;
        Ok(())
    }

    #[tracing::instrument(skip(self))]
    fn search_entries(&self, query: &str) -> Vec<&Self::Item> {
        let Some(ref inner) = self.inner else {
            debug!("searcher is not fully initialized, create the index first");
            return Vec::new();
        };

        let searcher = inner.reader.searcher();
        let query = inner.query_parser.parse_query_lenient(query).0;
        searcher
            .search(&query, &TopDocs::with_limit(30))
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
                        self.packages
                            .get(&name)
                            .expect("found option is not indexed")
                    })
                    .collect_vec()
            })
            .unwrap_or_default()
    }
}

struct ChannelSearcherInner {
    options: OptionsSearcher,
    packages: PackagesSearcher,
}

impl ChannelSearcherInner {
    /// attempt to load cached options
    pub fn maybe_load(branch_path: &Path) -> Option<Self> {
        let options =
            serde_json::from_str(&std::fs::read_to_string(branch_path.join("options.json")).ok()?)
                .ok()?;

        let packages =
            serde_json::from_str(&std::fs::read_to_string(branch_path.join("packages.json")).ok()?)
                .ok()?;

        Self::new_with_values(branch_path, options, packages)
    }

    pub fn new_with_values(
        branch_path: &Path,
        options: HashMap<String, NaiveNixosOption>,
        packages: HashMap<String, NixPackage>,
    ) -> Option<Self> {
        let options_index_path = branch_path.join("tantivy");
        let package_index_path = branch_path.join("tantivy_packages");

        let o_inner = OptionsSearcher::new_with_options(&options_index_path, options).ok()?;
        let p_inner = PackagesSearcher::new_with_packages(&package_index_path, packages).ok()?;

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
