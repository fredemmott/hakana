pub(crate) mod populator;

use crate::file_cache_provider::FileStatus;
use hakana_aast_helper::get_aast_for_path_and_contents;
use hakana_analyzer::config::Config;
use hakana_analyzer::file_analyzer;
use hakana_analyzer::taint_analyzer::find_tainted_data;
use hakana_file_info::FileSource;
use hakana_reflection_info::analysis_result::AnalysisResult;
use hakana_reflection_info::codebase_info::CodebaseInfo;
use hakana_reflection_info::data_flow::graph::GraphKind;
use hakana_reflection_info::issue::{Issue, IssueKind};
use hakana_reflection_info::member_visibility::MemberVisibility;
use indexmap::IndexMap;
use indicatif::{ProgressBar, ProgressStyle};
use oxidized::aast;
use oxidized::scoured_comments::ScouredComments;
use populator::populate_codebase;
use rust_embed::RustEmbed;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

mod file_cache_provider;

#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../../third-party/hhvm/hphp/hack/hhi"]
#[prefix = "hhi_embedded_"]
#[include = "*.hhi"]
#[include = "*.php"]
#[include = "*.hack"]
struct HhiAsset;

#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../../third-party/hhvm/hphp/hsl/src"]
#[prefix = "hsl_embedded_"]
#[include = "*.hhi"]
#[include = "*.php"]
#[include = "*.hack"]
struct HslAsset;

#[cfg(target_arch = "wasm32")]
#[derive(RustEmbed)]
#[folder = "$CARGO_MANIFEST_DIR/../../tests/security/taintedCurlInit/.hakana_cache"]
#[prefix = "cached_codebase_"]
#[include = "*codebase"]
struct CachedCodebase;

pub fn scan_and_analyze(
    include_core_libs: bool,
    stubs_dirs: Vec<String>,
    filter: Option<String>,
    ignored_paths: Option<FxHashSet<String>>,
    config: Arc<Config>,
    cache_dir: Option<&String>,
    threads: u8,
    debug: bool,
    header: &str,
    starter_codebase: Option<CodebaseInfo>,
) -> io::Result<AnalysisResult> {
    let mut all_scanned_dirs = stubs_dirs.clone();
    all_scanned_dirs.push(config.root_dir.clone());

    let now = Instant::now();

    let mut files_to_analyze = vec![];

    let (mut codebase, file_statuses) = scan_files(
        &all_scanned_dirs,
        include_core_libs,
        cache_dir,
        &mut files_to_analyze,
        &config,
        threads,
        debug,
        header,
        starter_codebase,
    )?;

    if let Some(cache_dir) = cache_dir {
        let timestamp_path = format!("{}/buildinfo", cache_dir);
        let mut timestamp_file = fs::File::create(&timestamp_path).unwrap();
        write!(timestamp_file, "{}", header).unwrap();

        let aast_manifest_path = format!("{}/manifest", cache_dir);
        let mut manifest_file = fs::File::create(&aast_manifest_path).unwrap();
        let mapped = file_statuses
            .iter()
            .filter(|(_, v)| match v {
                FileStatus::Deleted => false,
                _ => true,
            })
            .map(|(k, v)| {
                (
                    k.clone(),
                    match v {
                        FileStatus::Unchanged(a, b)
                        | FileStatus::Added(a, b)
                        | FileStatus::Modified(a, b) => (a, b),
                        FileStatus::Deleted => panic!(),
                    },
                )
            })
            .collect::<FxHashMap<_, _>>();
        let serialized_hashes = bincode::serialize(&mapped).unwrap();
        manifest_file
            .write_all(&serialized_hashes)
            .unwrap_or_else(|_| panic!("Could not write aast manifest {}", &aast_manifest_path));
    }

    let elapsed = now.elapsed();

    if debug {
        println!("File discovery & scanning took {:.2?}", elapsed);
    }

    println!("Calculating symbol inheritance");

    populate_codebase(&mut codebase);

    let now = Instant::now();

    let analysis_result = Arc::new(Mutex::new(AnalysisResult::new()));

    let arc_codebase = Arc::new(codebase);

    analyze_files(
        files_to_analyze,
        arc_codebase.clone(),
        config.clone(),
        &analysis_result,
        filter,
        &ignored_paths,
        None,
        &file_statuses,
        threads,
        debug,
    )?;

    let elapsed = now.elapsed();

    if debug {
        println!("File analysis took {:.2?}", elapsed);
    }

    let mut analysis_result = (*analysis_result.lock().unwrap()).clone();

    if config.find_unused_definitions {
        find_unused_definitions(
            &mut analysis_result,
            &config,
            arc_codebase.clone(),
            &ignored_paths,
        );
    }

    std::thread::spawn(move || drop(arc_codebase));

    if config.graph_kind == GraphKind::WholeProgram {
        let issues = find_tainted_data(&analysis_result.taint_flow_graph, &config, debug);

        for issue in issues {
            analysis_result
                .emitted_issues
                .entry((*issue.pos.file_path).clone())
                .or_insert_with(Vec::new)
                .push(issue);
        }
    }

    Ok(analysis_result)
}

fn find_unused_definitions(
    analysis_result: &mut AnalysisResult,
    config: &Arc<Config>,
    arc_codebase: Arc<CodebaseInfo>,
    ignored_paths: &Option<FxHashSet<String>>,
) {
    let referenced_symbols = analysis_result.symbol_references.get_referenced_symbols();
    let referenced_class_members = analysis_result
        .symbol_references
        .get_referenced_class_members();
    let referenced_overridden_class_members = analysis_result
        .symbol_references
        .get_referenced_overridden_class_members();

    'outer1: for (function_name, functionlike_info) in &arc_codebase.functionlike_infos {
        if functionlike_info.user_defined
            && !functionlike_info.dynamically_callable
            && !functionlike_info.generated
        {
            let pos = functionlike_info.name_location.as_ref().unwrap();

            if let Some(ignored_paths) = ignored_paths {
                for ignored_path in ignored_paths {
                    if pos.file_path.matches(ignored_path.as_str()).count() > 0 {
                        continue 'outer1;
                    }
                }
            }

            if !referenced_symbols.contains(function_name) {
                if let Some(suppressed_issues) = &functionlike_info.suppressed_issues {
                    if suppressed_issues.contains_key(&IssueKind::UnusedFunction) {
                        continue;
                    }
                }

                if !config.allow_issue_kind_in_file(&IssueKind::UnusedFunction, &pos.file_path) {
                    continue;
                }

                if config
                    .migration_symbols
                    .contains(&("unused_symbol".to_string(), function_name.clone()))
                {
                    if let Some(def_pos) = &functionlike_info.def_location {
                        analysis_result
                            .replacements
                            .entry((*pos.file_path).clone())
                            .or_insert_with(BTreeMap::new)
                            .insert((def_pos.start_offset, def_pos.end_offset), "".to_string());
                    }
                }

                let issue = Issue::new(
                    IssueKind::UnusedFunction,
                    format!("Unused function {}", function_name),
                    pos.clone(),
                );

                if config.can_add_issue(&issue) {
                    analysis_result
                        .emitted_issues
                        .entry((*pos.file_path).clone())
                        .or_insert_with(Vec::new)
                        .push(issue);
                }
            }
        }
    }

    'outer2: for (classlike_name, classlike_info) in &arc_codebase.classlike_infos {
        if classlike_info.user_defined && !classlike_info.generated {
            let pos = classlike_info.name_location.as_ref().unwrap();

            if let Some(ignored_paths) = ignored_paths {
                for ignored_path in ignored_paths {
                    if pos.file_path.matches(ignored_path.as_str()).count() > 0 {
                        continue 'outer2;
                    }
                }
            }

            if !config.allow_issue_kind_in_file(&IssueKind::UnusedClass, &pos.file_path) {
                continue;
            }

            for parent_class in &classlike_info.all_parent_classes {
                if let Some(parent_classlike_info) = arc_codebase.classlike_infos.get(parent_class)
                {
                    if !parent_classlike_info.user_defined {
                        continue 'outer2;
                    }
                }
            }

            if !referenced_symbols.contains(classlike_name) {
                let issue = Issue::new(
                    IssueKind::UnusedClass,
                    format!("Unused class, interface or enum {}", classlike_name),
                    pos.clone(),
                );

                if config.can_add_issue(&issue) {
                    analysis_result
                        .emitted_issues
                        .entry((*pos.file_path).clone())
                        .or_insert_with(Vec::new)
                        .push(issue);
                }
            } else {
                'inner: for (method_name, functionlike_storage) in &classlike_info.methods {
                    if method_name.starts_with("__") && method_name != "__construct" {
                        continue;
                    }

                    let pair = (classlike_name.clone(), format!("{}()", method_name));

                    if !referenced_class_members.contains(&pair)
                        && !referenced_overridden_class_members.contains(&pair)
                    {
                        if let Some(parent_elements) =
                            classlike_info.overridden_method_ids.get(method_name)
                        {
                            for parent_element in parent_elements {
                                if referenced_class_members.contains(&(
                                    (*parent_element).clone(),
                                    format!("{}()", method_name),
                                )) {
                                    continue 'inner;
                                }
                            }
                        }

                        let method_storage = functionlike_storage.method_info.as_ref().unwrap();

                        if let Some(suppressed_issues) = &functionlike_storage.suppressed_issues {
                            if suppressed_issues.contains_key(&IssueKind::UnusedPrivateMethod) {
                                continue;
                            }
                        }

                        // allow one-liner private construct statements that prevent instantiation
                        if method_name == "__construct"
                            && matches!(method_storage.visibility, MemberVisibility::Private)
                        {
                            if let (Some(stmt_pos), Some(name_pos)) = (
                                &functionlike_storage.def_location,
                                &functionlike_storage.name_location,
                            ) {
                                if stmt_pos.end_line - name_pos.start_line <= 1 {
                                    continue;
                                }
                            }
                        }

                        let issue =
                            if matches!(method_storage.visibility, MemberVisibility::Private) {
                                Issue::new(
                                    IssueKind::UnusedPrivateMethod,
                                    format!("Unused method {}::{}", classlike_name, method_name),
                                    functionlike_storage.name_location.clone().unwrap(),
                                )
                            } else {
                                Issue::new(
                                    IssueKind::UnusedPublicOrProtectedMethod,
                                    format!(
                                        "Possibly-unused method {}::{}",
                                        classlike_name, method_name
                                    ),
                                    functionlike_storage.name_location.clone().unwrap(),
                                )
                            };

                        if !config.allow_issue_kind_in_file(&issue.kind, &pos.file_path) {
                            continue;
                        }

                        if config.can_add_issue(&issue) {
                            analysis_result
                                .emitted_issues
                                .entry((*pos.file_path).clone())
                                .or_insert_with(Vec::new)
                                .push(issue);
                        }
                    }
                }
            }
        }
    }
}

pub fn scan_and_analyze_single_file(
    codebase: &mut CodebaseInfo,
    file_name: String,
    file_contents: String,
    find_unused_expressions: bool,
) -> std::result::Result<AnalysisResult, String> {
    let mut analysis_config = Config::new("".to_string());
    analysis_config.find_unused_expressions = find_unused_expressions;
    analysis_config.graph_kind = if file_contents.starts_with("// security-check")
        || file_contents.starts_with("//security-check")
    {
        GraphKind::WholeProgram
    } else {
        GraphKind::FunctionBody
    };

    scan_single_file(codebase, file_name.clone(), file_contents.clone())?;

    populate_codebase(codebase);

    let mut analysis_result = analyze_single_file(
        file_name.clone(),
        file_contents.clone(),
        &codebase,
        &analysis_config,
    )?;

    if analysis_config.graph_kind == GraphKind::WholeProgram {
        let issues = find_tainted_data(&analysis_result.taint_flow_graph, &analysis_config, false);

        for issue in issues {
            analysis_result
                .emitted_issues
                .entry((*issue.pos.file_path).clone())
                .or_insert_with(Vec::new)
                .push(issue);
        }
    }

    Ok(analysis_result)
}

pub fn scan_files(
    scan_dirs: &Vec<String>,
    include_core_libs: bool,
    cache_dir: Option<&String>,
    files_to_analyze: &mut Vec<String>,
    config: &Config,
    threads: u8,
    debug: bool,
    build_checksum: &str,
    starter_codebase: Option<CodebaseInfo>,
) -> io::Result<(CodebaseInfo, IndexMap<String, FileStatus>)> {
    if debug {
        println!("{:#?}", scan_dirs);
    }

    let mut files_to_scan = IndexMap::new();

    let codebase_path = if let Some(cache_dir) = cache_dir {
        Some(format!("{}/codebase", cache_dir))
    } else {
        None
    };

    let mut codebase = starter_codebase.unwrap_or(CodebaseInfo::new());

    if include_core_libs {
        // add HHVM libs
        for file in HhiAsset::iter() {
            files_to_scan.insert(file.to_string(), 0);
        }

        // add HSL
        for file in HslAsset::iter() {
            files_to_scan.insert(file.to_string(), 0);
        }
    }

    for scan_dir in scan_dirs {
        if debug {
            println!("Looking for Hack files in {}", scan_dir);
        }

        files_to_scan.extend(find_files_in_dir(scan_dir, config, files_to_analyze));
    }

    let mut use_codebase_cache = true;

    if let (Some(cache_dir), Some(codebase_path_unwrapped)) = (cache_dir, codebase_path.clone()) {
        let build_checksum_path = format!("{}/buildinfo", cache_dir);
        let build_checksum_path = Path::new(&build_checksum_path);

        if build_checksum_path.exists() {
            if let Ok(contents) = fs::read_to_string(build_checksum_path) {
                if contents != build_checksum {
                    use_codebase_cache = false;
                }
            } else {
                use_codebase_cache = false;
            }
        } else {
            use_codebase_cache = false;
        }

        if !use_codebase_cache {
            if Path::new(&codebase_path_unwrapped).exists() {
                fs::remove_file(&codebase_path_unwrapped).unwrap();
            }
        }
    }

    let file_update_hashes = if let Some(cache_dir) = cache_dir {
        if use_codebase_cache {
            file_cache_provider::get_file_manifest(cache_dir).unwrap_or(FxHashMap::default())
        } else {
            FxHashMap::default()
        }
    } else {
        FxHashMap::default()
    };

    let file_statuses = file_cache_provider::get_file_diff(&files_to_scan, file_update_hashes);

    if let Some(codebase_path) = &codebase_path {
        load_cached_codebase(
            codebase_path,
            use_codebase_cache,
            &mut codebase,
            &file_statuses,
        );
    }

    let mut files_to_scan = vec![];

    for (target_file, status) in &file_statuses {
        if matches!(status, FileStatus::Added(..) | FileStatus::Modified(..)) {
            files_to_scan.push(target_file);
        }
    }

    if files_to_scan.len() > 0 {
        let bar = if debug {
            None
        } else {
            let pb = ProgressBar::new(files_to_scan.len() as u64);
            let sty =
                ProgressStyle::with_template("{bar:40.green/yellow} {pos:>7}/{len:7}").unwrap();
            pb.set_style(sty);
            Some(Arc::new(pb))
        };

        let files_processed: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));

        println!("Scanning {} files", files_to_scan.len());

        let mut group_size = threads as usize;

        let mut path_groups = FxHashMap::default();

        if (files_to_scan.len() / group_size) < 4 {
            group_size = 1;
        }

        for (i, str_path) in files_to_scan.into_iter().enumerate() {
            let group = i % group_size;
            path_groups
                .entry(group)
                .or_insert_with(Vec::new)
                .push(str_path);
        }

        if path_groups.len() == 1 {
            let mut new_codebase = CodebaseInfo::new();

            let analyze_map = files_to_analyze
                .clone()
                .into_iter()
                .collect::<FxHashSet<_>>();

            for (i, str_path) in path_groups[&0].iter().enumerate() {
                scan_file(
                    str_path,
                    &config.root_dir,
                    &mut new_codebase,
                    analyze_map.contains(*str_path),
                    debug,
                );

                update_progressbar(i as u64, bar.clone());
            }

            codebase.extend(new_codebase);
        } else {
            let mut handles = vec![];

            let thread_codebases = Arc::new(Mutex::new(vec![]));

            for (_, path_group) in path_groups {
                let pgc = path_group
                    .iter()
                    .map(|c| c.clone().clone())
                    .collect::<Vec<_>>();

                let root_dir_c = config.root_dir.clone();

                let codebases = thread_codebases.clone();

                let bar = bar.clone();
                let files_processed = files_processed.clone();

                let analyze_map = files_to_analyze
                    .clone()
                    .into_iter()
                    .collect::<FxHashSet<_>>();

                let handle = std::thread::spawn(move || {
                    let mut new_codebase = CodebaseInfo::new();

                    for str_path in &pgc {
                        scan_file(
                            str_path,
                            &root_dir_c,
                            &mut new_codebase,
                            analyze_map.contains(str_path),
                            debug,
                        );

                        let mut tally = files_processed.lock().unwrap();
                        *tally += 1;

                        update_progressbar(*tally, bar.clone());
                    }

                    let mut codebases = codebases.lock().unwrap();
                    codebases.push(new_codebase);
                });

                handles.push(handle);
            }

            for handle in handles {
                handle.join().unwrap();
            }

            if let Ok(thread_codebases) = Arc::try_unwrap(thread_codebases) {
                for thread_codebase in thread_codebases.into_inner().unwrap().into_iter() {
                    codebase.extend(thread_codebase.clone());
                }
            }
        }

        if let Some(bar) = &bar {
            bar.finish_and_clear();
        }

        if let Some(codebase_path) = codebase_path {
            let mut codebase_file = fs::File::create(&codebase_path).unwrap();
            let serialized_codebase = bincode::serialize(&codebase).unwrap();
            codebase_file.write_all(&serialized_codebase)?;
        }
    }

    Ok((codebase, file_statuses))
}

fn load_cached_codebase(
    codebase_path: &String,
    use_codebase_cache: bool,
    codebase: &mut CodebaseInfo,
    file_statuses: &IndexMap<String, FileStatus>,
) {
    if Path::new(codebase_path).exists() && use_codebase_cache {
        println!("Deserializing stored codebase cache");
        let serialized = fs::read(&codebase_path)
            .unwrap_or_else(|_| panic!("Could not read file {}", &codebase_path));
        if let Ok(d) = bincode::deserialize::<CodebaseInfo>(&serialized) {
            *codebase = d;

            let changed_files = file_statuses
                .iter()
                .filter(|(_, v)| !matches!(v, FileStatus::Unchanged(..)))
                .map(|(k, _)| k)
                .collect::<FxHashSet<_>>();

            let functions_to_remove = codebase
                .functions_in_files
                .iter()
                .filter(|(k, _)| changed_files.contains(k))
                .map(|(_, v)| v.clone().into_iter().collect::<Vec<_>>())
                .flatten()
                .collect::<FxHashSet<_>>();

            let typedefs_to_remove = codebase
                .typedefs_in_files
                .iter()
                .filter(|(k, _)| changed_files.contains(k))
                .map(|(_, v)| v.clone().into_iter().collect::<Vec<_>>())
                .flatten()
                .collect::<FxHashSet<_>>();

            let constants_to_remove = codebase
                .const_files
                .iter()
                .filter(|(k, _)| changed_files.contains(k))
                .map(|(_, v)| v.clone().into_iter().collect::<Vec<_>>())
                .flatten()
                .collect::<FxHashSet<_>>();

            let classlikes_to_remove = codebase
                .classlikes_in_files
                .iter()
                .filter(|(k, _)| changed_files.contains(k))
                .map(|(_, v)| v.clone().into_iter().collect::<Vec<_>>())
                .flatten()
                .collect::<FxHashSet<_>>();

            codebase
                .functionlike_infos
                .retain(|k, _| !functions_to_remove.contains(k));

            codebase
                .type_definitions
                .retain(|k, _| !typedefs_to_remove.contains(k));

            codebase
                .constant_infos
                .retain(|k, _| !constants_to_remove.contains(k));

            codebase
                .classlike_infos
                .retain(|k, _| !classlikes_to_remove.contains(k));
        }
    }
}

fn find_files_in_dir(
    scan_dir: &String,
    config: &Config,
    files_to_analyze: &mut Vec<String>,
) -> IndexMap<String, u64> {
    let mut files_to_scan = IndexMap::new();

    let mut walker_builder = ignore::WalkBuilder::new(scan_dir);

    walker_builder
        .sort_by_file_path(|a, b| a.file_name().cmp(&b.file_name()))
        .follow_links(true);

    for ignore_path in &config.ignore_files {
        walker_builder.add_ignore(Path::new(ignore_path));
    }

    let walker = walker_builder.build().into_iter().filter_map(|e| e.ok());

    for entry in walker {
        let path = entry.path();

        let metadata = if let Ok(metadata) = fs::metadata(&path) {
            metadata
        } else {
            println!("Could not get metadata");
            panic!();
        };

        if metadata.is_file() {
            if let Some(extension) = path.extension() {
                if extension.eq("hack") || extension.eq("php") || extension.eq("hhi") {
                    let path = path.to_str().unwrap().to_string();

                    files_to_scan.insert(
                        path.clone(),
                        metadata
                            .modified()
                            .unwrap()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap()
                            .as_secs(),
                    );

                    if !extension.eq("hhi") {
                        if config.graph_kind == GraphKind::WholeProgram {
                            if config.allow_taints_in_file(&path) {
                                files_to_analyze.push(path.clone());
                            }
                        } else {
                            files_to_analyze.push(path.clone());
                        }
                    }
                }
            }
        }
    }

    files_to_scan
}

pub fn get_aast_for_path(
    path: &String,
    root_dir: &String,
    cache_dir: Option<&String>,
    has_changed: bool,
) -> Result<(aast::Program<(), ()>, ScouredComments), String> {
    let file_contents = if path.starts_with("hsl_embedded_") {
        std::str::from_utf8(
            &HslAsset::get(path)
                .unwrap_or_else(|| panic!("Could not read HSL file {}", path))
                .data,
        )
        .unwrap_or_else(|_| panic!("Could not convert HSL file {}", path))
        .to_string()
    } else if path.starts_with("hhi_embedded_") {
        std::str::from_utf8(
            &HhiAsset::get(path)
                .unwrap_or_else(|| panic!("Could not read HSL file {}", path))
                .data,
        )
        .unwrap_or_else(|_| panic!("Could not convert HHI file {}", path))
        .to_string()
    } else if path.ends_with("tests/stubs/stubs.hack") {
        "function hakana_expect_type<T>(T $id): void {}".to_string()
    } else {
        fs::read_to_string(path).unwrap_or_else(|_| panic!("Could not read file {}", path))
    };

    let mut local_path = path.clone();

    if local_path.starts_with(root_dir) {
        local_path = local_path.replace(root_dir, "");
        local_path = local_path[1..].to_string();
    }

    let aast_cache_dir = if let Some(cache_dir) = cache_dir {
        Some(format!("{}/ast", cache_dir))
    } else {
        None
    };

    get_aast_for_path_and_contents(local_path, file_contents, aast_cache_dir, has_changed)
}

fn scan_file(
    target_file: &String,
    root_dir: &String,
    codebase: &mut CodebaseInfo,
    user_defined: bool,
    debug: bool,
) {
    if debug {
        println!("scanning {}", &target_file);
    }

    let aast = get_aast_for_path(&target_file, root_dir, None, true);

    let aast = if let Ok(aast) = aast {
        aast
    } else {
        return;
    };

    let target_name = if target_file.contains(root_dir) {
        target_file[(root_dir.len() + 1)..].to_string()
    } else {
        target_file.clone()
    };

    let resolved_names = hakana_aast_helper::scope_names(&aast.0);

    hakana_reflector::collect_info_for_aast(
        &aast.0,
        resolved_names,
        codebase,
        FileSource {
            file_path: Arc::new(target_name),
            hh_fixmes: aast.1.fixmes,
            comments: aast.1.comments,
        },
        user_defined,
    );
}

#[cfg(not(target_arch = "wasm32"))]
pub fn get_single_file_codebase(additional_files: Vec<&str>) -> CodebaseInfo {
    let mut codebase = CodebaseInfo::new();

    // add HHVM libs
    for file in HhiAsset::iter() {
        scan_file(
            &file.to_string(),
            &"".to_string(),
            &mut codebase,
            false,
            false,
        );
    }

    // add HHVM libs
    for file in HslAsset::iter() {
        scan_file(
            &file.to_string(),
            &"".to_string(),
            &mut codebase,
            false,
            false,
        );
    }

    for str_path in additional_files {
        scan_file(
            &str_path.to_string(),
            &"".to_string(),
            &mut codebase,
            false,
            false,
        );
    }

    populate_codebase(&mut codebase);

    codebase
}

#[cfg(target_arch = "wasm32")]
pub fn get_single_file_codebase(additional_files: Vec<&str>) -> CodebaseInfo {
    let mut codebase = CodebaseInfo::new();

    // add HHVM libs
    for file_path in CachedCodebase::iter() {
        let serialized = CachedCodebase::get(&file_path)
            .unwrap_or_else(|| panic!("Could not read HSL file {}", file_path))
            .data;
        if let Ok(d) = bincode::deserialize::<CodebaseInfo>(&serialized) {
            codebase = d;
        }
    }

    populate_codebase(&mut codebase);

    codebase
}

pub fn scan_single_file(
    codebase: &mut CodebaseInfo,
    path: String,
    file_contents: String,
) -> std::result::Result<(), String> {
    let aast = match get_aast_for_path_and_contents(path.clone(), file_contents, None, true) {
        Ok(aast) => aast,
        Err(err) => return std::result::Result::Err(format!("Unable to parse AAST\n{}", err)),
    };

    let resolved_names = hakana_aast_helper::scope_names(&aast.0);

    hakana_reflector::collect_info_for_aast(
        &aast.0,
        resolved_names.clone(),
        codebase,
        FileSource {
            file_path: Arc::new(path.clone()),
            hh_fixmes: aast.1.fixmes,
            comments: aast.1.comments,
        },
        true,
    );

    Ok(())
}

pub fn analyze_files(
    mut paths: Vec<String>,
    codebase: Arc<CodebaseInfo>,
    config: Arc<Config>,
    analysis_result: &Arc<Mutex<AnalysisResult>>,
    filter: Option<String>,
    ignored_paths: &Option<FxHashSet<String>>,
    cache_dir: Option<&String>,
    _file_statuses: &IndexMap<String, FileStatus>,
    threads: u8,
    debug: bool,
) -> io::Result<()> {
    let mut group_size = threads as usize;

    let mut path_groups = FxHashMap::default();

    if let Some(filter) = filter {
        paths.retain(|str_path| str_path.matches(filter.as_str()).count() > 0);
    }

    if let Some(ignored_paths) = &ignored_paths {
        for ignored_path in ignored_paths {
            paths.retain(|str_path| str_path.matches(ignored_path.as_str()).count() == 0);
        }
    }

    let total_file_count = paths.len() as u64;

    println!("Analyzing {} files", total_file_count);

    if (paths.len() / group_size) < 4 {
        group_size = 1;
    }

    for (i, str_path) in paths.iter().enumerate() {
        let group = i % group_size;
        path_groups
            .entry(group)
            .or_insert_with(Vec::new)
            .push(str_path);
    }

    let bar = if debug {
        None
    } else {
        let pb = ProgressBar::new(total_file_count);
        let sty = ProgressStyle::with_template("{bar:40.green/yellow} {pos:>7}/{len:7}").unwrap();
        pb.set_style(sty);
        Some(Arc::new(pb))
    };

    if path_groups.len() == 1 {
        let mut new_analysis_result = AnalysisResult::new();

        for (i, str_path) in path_groups[&0].iter().enumerate() {
            analyze_file(
                str_path,
                cache_dir,
                false,
                &codebase,
                &config,
                &mut new_analysis_result,
                debug,
            );

            update_progressbar(i as u64, bar.clone());
        }

        let mut a = analysis_result.lock().unwrap();
        *a = new_analysis_result;
    } else {
        let mut handles = vec![];

        let files_processed = Arc::new(Mutex::new(0));

        for (_, path_group) in path_groups {
            let codebase = codebase.clone();

            let pgc = path_group
                .iter()
                .map(|c| c.clone().clone())
                .collect::<Vec<_>>();

            let cache_dir_c = cache_dir.cloned();

            let analysis_result = analysis_result.clone();

            let analysis_config = config.clone();

            let files_processed = files_processed.clone();
            let bar = bar.clone();

            let handle = std::thread::spawn(move || {
                let mut new_analysis_result = AnalysisResult::new();

                for str_path in &pgc {
                    analyze_file(
                        str_path,
                        cache_dir_c.as_ref(),
                        false,
                        &codebase,
                        &analysis_config,
                        &mut new_analysis_result,
                        debug,
                    );

                    let mut tally = files_processed.lock().unwrap();
                    *tally += 1;

                    update_progressbar(*tally, bar.clone());
                }

                let mut a = analysis_result.lock().unwrap();
                a.extend(new_analysis_result);
            });

            handles.push(handle);
        }

        for handle in handles {
            handle.join().unwrap();
        }
    }

    if let Some(bar) = &bar {
        bar.finish_and_clear();
    }

    Ok(())
}

fn update_progressbar(percentage: u64, bar: Option<Arc<ProgressBar>>) {
    if let Some(bar) = bar {
        bar.set_position(percentage);
    }
}

fn analyze_file(
    str_path: &String,
    cache_dir: Option<&String>,
    has_changed: bool,
    codebase: &Arc<CodebaseInfo>,
    config: &Arc<Config>,
    analysis_result: &mut AnalysisResult,
    debug: bool,
) {
    if debug {
        println!("analyzing {}", &str_path);
    }

    let aast_result = get_aast_for_path(str_path, &config.root_dir, cache_dir, has_changed);
    let aast = match aast_result {
        Ok(aast) => aast,
        Err(error) => {
            println!("invalid syntax in {}\n{}\n", str_path, error);
            return;
        }
    };
    let resolved_names = hakana_aast_helper::scope_names(&aast.0);

    let target_name = if str_path.contains(&config.root_dir) {
        str_path[(config.root_dir.len() + 1)..].to_string()
    } else {
        str_path.clone()
    };

    let file_source = FileSource {
        file_path: Arc::new(target_name),
        hh_fixmes: aast.1.fixmes,
        comments: aast.1.comments,
    };
    let mut file_analyzer =
        file_analyzer::FileAnalyzer::new(file_source, &resolved_names, codebase, config);
    file_analyzer.analyze(&aast.0, analysis_result);
}

pub fn analyze_single_file(
    path: String,
    file_contents: String,
    codebase: &CodebaseInfo,
    analysis_config: &Config,
) -> std::result::Result<AnalysisResult, String> {
    let aast_result = get_aast_for_path_and_contents(path.clone(), file_contents, None, true);

    let aast = match aast_result {
        Ok(aast) => aast,
        Err(error) => {
            return std::result::Result::Err(error);
        }
    };

    let resolved_names = hakana_aast_helper::scope_names(&aast.0);

    let mut analysis_result = AnalysisResult::new();

    let file_source = FileSource {
        file_path: Arc::new(path.clone()),
        hh_fixmes: aast.1.fixmes,
        comments: aast.1.comments,
    };

    let mut file_analyzer =
        file_analyzer::FileAnalyzer::new(file_source, &resolved_names, codebase, analysis_config);

    file_analyzer.analyze(&aast.0, &mut analysis_result);

    Ok(analysis_result)
}
