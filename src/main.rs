//! Recall's engine as a command. Every command can work on an explicit index folder (--index), so tests never touch
//! the real one.

mod cli;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use serde_json::json;

use cli::{bytes, duration, emit, file_name, folder_path, is_folder, is_readable, location, lpad, number, pad,
          parent_folder, skipped_line, states_line, tilde, Arguments};
use recall::crawler;
use recall::estimator::{self, Costs, Estimate};
use recall::indexer::Indexer;
use recall::llama;
use recall::model::{code_query, prompts, FileKind, SegmentKind};
use recall::model_files;
use recall::paths;
use recall::searcher::{self, SearchHit, Searcher};
use recall::store::{self, Store};
use recall::text;

fn usage() -> String {
    format!(
        "recall-engine: search your files by what's in them (the engine as a command)

  recall-engine estimate <folder>... [--json]    what would be indexed, and how long the first pass takes
  recall-engine index <folder>... [--limit N]    sync the index with these folders, embed what's new, then the rest
                                          of long PDFs and documents
  recall-engine search <words>... [--kind K,K] [-n 10] [--json]
                                          \"code: <words>\" searches the code index
  recall-engine devices                          JSON diagnostics: selected GPU/CPU and backend plugins
  recall-engine status                           what's in the index
  recall-engine eval <queries.json> [-n 10]      recall of known answers (see evals/; \"code: …\" queries search code)
  recall-engine index-service <folder>... [--code-root DIR]... [--code-exclude DIR]... [--backfill] [--prune]
                                          the app's indexing helper (JSON lines)
  recall-engine worker [--text-only]             the model as a process (JSON lines)
  recall-engine serve [--idle 600]               search as a process, for the app (JSON lines)

Options:
  --index DIR          index folder (default: {})
  --code               estimate, index or show the code index of these folders (code.sqlite in the index
                       folder): source files, their repos' docs and notebooks, minus what git ignores
  --kind K,K           screenshot, image, pdf, doc, audio, video
  --include-code       also index loose source code and data files as documents (outside code projects)
  --include-datasets   don't skip folders that look like datasets
  --exclude DIR        don't index this folder (repeatable)
  --exclude-ext EXT    don't index files with this extension (repeatable, or comma-separated: \"heic,txt\")
  --reindex            drop vectors and embed everything again (after changing settings or model)
  --zgate Z            how far a meaning match must stand out (default 2.0)
  --word-weight W      bonus for containing all of the query's words (default 2.0, 1.0 for code)
  --shot-tokens N      vision tokens per screenshot (default 280)
  --code-stretch N     characters a stretch of code holds at most (default 1800)

The model: EmbeddingGemma 2 on llama.cpp, from $RECALL_MODELS (only there, when set), else
the pinned snapshot in the Hugging Face cache ({}).
RECALL_DEVICE=cpu|cuda|vulkan picks where it runs (default: CUDA, else Vulkan, else the CPU).",
        paths::tilde(&paths::default_index()),
        paths::tilde(&model_files::hub_snapshot())
    )
}

// MARK: Estimate

fn files_json(estimate: &Estimate) -> serde_json::Value {
    estimate.files.iter().map(|(kind, count)| (kind.as_str().to_string(), json!(count))).collect()
}

fn estimate(args: &Arguments) -> Result<()> {
    if args.positional.is_empty() {
        bail!("which folders? e.g. recall estimate ~/Documents");
    }
    let options = args.options();
    let costs = Costs::default();
    let as_json = args.flag("--json");
    let mut total = 0.0;
    for folder in args.positional.iter().map(|path| folder_path(path)) {
        let folder_text = folder.to_string_lossy().into_owned();
        // One folder that isn't there or can't be read mustn't sink the others.
        if !is_folder(&folder) || !is_readable(&folder) {
            if as_json {
                emit(json!({"folder": folder_text, "readable": false, "seconds": 0, "files": {}, "pdf_pages": 0,
                            "audio_seconds": 0, "video_seconds": 0}));
            } else {
                println!("{}    can't read it (not there, or no permission)", tilde(&folder_text));
            }
            continue;
        }
        let started = Instant::now();
        let crawl = crawler::crawl(std::slice::from_ref(&folder), &options);
        let parts = estimator::estimates_by_subfolder(&crawl.candidates, &folder_text, &options, &costs);
        let estimate = parts.values().cloned().fold(Estimate::default(), |sum, part| sum + part);
        // Biggest first; the app lists them under the folder, so that a long first pass can be cut down.
        let mut subfolders: Vec<(&String, &Estimate)> = parts.iter().filter(|(key, _)| **key != folder_text).collect();
        subfolders.sort_by(|a, b| b.1.seconds.total_cmp(&a.1.seconds).then(a.0.cmp(b.0)));
        total += estimate.seconds;
        if as_json {
            // One line per folder, as soon as it's done (the app's onboarding shows them as they come).
            let rows: Vec<serde_json::Value> = subfolders
                .iter()
                .take(500)
                .map(|(path, part)| {
                    json!({"folder": path, "seconds": part.seconds, "files": files_json(part),
                           "long_files": part.long_files, "later_seconds": part.later_seconds, "bytes": part.bytes})
                })
                .collect();
            emit(json!({
                "folder": folder_text, "seconds": estimate.seconds, "readable": true, "files": files_json(&estimate),
                "pdf_pages": estimate.pdf_pages, "pdf_pages_total": estimate.pdf_pages_total,
                "audio_seconds": estimate.audio_seconds, "video_seconds": estimate.video_seconds,
                "long_files": estimate.long_files, "later_seconds": estimate.later_seconds,
                "code_stretches": estimate.code_stretches, "bytes": estimate.bytes, "subfolders": rows,
                "skipped": crawl.skipped.values().sum::<usize>(), "datasets": crawl.datasets.len(),
                "ms": started.elapsed().as_millis() as u64,
            }));
            continue;
        }
        let mut kinds = Vec::new();
        for kind in FileKind::ALL {
            let Some(&count) = estimate.files.get(&kind).filter(|count| **count > 0) else { continue };
            kinds.push(match kind {
                FileKind::Pdf => format!("{} PDFs ({} pages; {} in the first pass)", number(count),
                                         number(estimate.pdf_pages_total), number(estimate.pdf_pages)),
                FileKind::Audio => format!("{} audio ({})", number(count), duration(estimate.audio_seconds)),
                FileKind::Video => format!("{} video ({})", number(count), duration(estimate.video_seconds)),
                FileKind::Code => format!("{} code {} ({} stretches)", number(count),
                                          if count == 1 { "file" } else { "files" }, number(estimate.code_stretches)),
                _ => format!("{} {}{}", number(count), kind.as_str(), if count == 1 { "" } else { "s" }),
            });
        }
        println!("{}    first index ≈ {}{}", tilde(&folder_text), duration(estimate.seconds),
                 if args.code() { format!(", index ≈ {}", bytes(estimate.bytes)) } else { String::new() });
        println!("  {}", if kinds.is_empty() { "nothing to index".to_string() } else { kinds.join(" · ") });
        if estimate.long_files > 0 {
            println!("  then the rest of {} long {} ≈ {}", number(estimate.long_files),
                     if estimate.long_files == 1 { "file" } else { "files" }, duration(estimate.later_seconds));
        }
        if !subfolders.is_empty() {
            let names: Vec<String> = subfolders
                .iter()
                .take(5)
                .map(|(path, part)| format!("{} ≈ {}", file_name(path), duration(part.seconds)))
                .collect();
            let more = if subfolders.len() > 5 { format!(" · {} more", subfolders.len() - 5) } else { String::new() };
            println!("  biggest subfolders: {}{more}", names.join(" · "));
        }
        if !crawl.skipped.is_empty() {
            println!("  skipped: {}", skipped_line(&crawl.skipped));
        }
        if !crawl.skipped_folders.is_empty() {
            println!("  folders not walked: {}", skipped_line(&crawl.skipped_folders));
        }
        let mut datasets: Vec<(&String, &usize)> = crawl.datasets.iter().collect();
        datasets.sort_by(|a, b| b.1.cmp(a.1));
        for (folder, count) in datasets {
            println!("  looks like a dataset (skipped): {} ({} files)", tilde(folder), number(*count));
        }
        println!("  (looked at it in {:.2} s)", started.elapsed().as_secs_f64());
    }
    if args.positional.len() > 1 && !as_json {
        println!("all together ≈ {}", duration(total));
    }
    Ok(())
}

// MARK: Index

fn index(args: &Arguments) -> Result<()> {
    let store = args.store()?;
    if args.flag("--reindex") {
        store.reset_all()?;
    }
    let file = store.file.to_string_lossy().into_owned();
    let log = store.directory.join("worker.log");
    let mut indexer = Indexer::new(store, args.options(), Box::new(move || cli::make_embedder(false, &log)))?;
    indexer.embedder_source = cli::embedder_source();
    indexer.check_model()?;
    let folders = args.folders()?;
    let sync = indexer.sync(&folders)?;
    println!("index: {}", tilde(&file));
    println!("sync: {} new · {} changed · {} moved · {} removed · {} unchanged", sync.added, sync.changed, sync.moved,
             sync.removed, sync.unchanged);
    if !sync.crawl.skipped.is_empty() {
        println!("skipped: {}", skipped_line(&sync.crawl.skipped));
    }
    for (folder, count) in &sync.crawl.datasets {
        println!("looks like a dataset (skipped): {} ({} files)", tilde(folder), number(*count));
    }
    let limit: Option<usize> = args.number("--limit");
    let started = Instant::now();
    let states = indexer.run(limit, None, &mut || false, &mut |event| {
        let note = event.note.as_ref().map(|note| format!("  ({note})")).unwrap_or_default();
        println!("[{}/{}] {} {} {:5.2}s  {}{note}", lpad(&event.done.to_string(), 4), event.total,
                 pad(event.kind.as_str(), 10), pad(&event.state, 7), event.seconds, file_name(&event.path));
    })?;
    if states.is_empty() {
        println!("nothing pending");
    } else {
        println!("done in {}: {}", duration(started.elapsed().as_secs_f64()), states_line(&states));
    }
    // The rest of long files, as the app does once nothing is pending (not after a --limit run: things still are).
    if limit.is_some() || indexer.store.unfinished_count()?.0 == 0 {
        return Ok(());
    }
    let started = Instant::now();
    let finished = indexer.finish_long_files(&mut || false, &mut |step| {
        let note = step.note.as_ref().map(|note| format!("  ({note})")).unwrap_or_default();
        println!("[rest {}/{}] {} {} {:5.2}s  {}{note}", lpad(&step.done.to_string(), 5), step.total,
                 pad(step.kind.as_str(), 4), pad(&step.state, 7), step.seconds, file_name(&step.path));
    })?;
    println!("the rest of long files in {}: {}", duration(started.elapsed().as_secs_f64()),
             if finished.is_empty() { "none finished".to_string() } else { states_line(&finished) });
    Ok(())
}

// MARK: Search

/// Embeds queries with the text model alone (~0.25 GB): the query encoder. `code`: the code index's prompt.
fn query_vectors(queries: &[String], store: &Store, searcher: &Searcher, code: bool) -> Result<Option<Vec<Vec<f32>>>> {
    if searcher.vector_count() == 0 || queries.is_empty() {
        return Ok(None);
    }
    let mut worker = cli::make_embedder(true, &store.directory.join("worker-query.log"))?;
    let source = worker.info().vector_source();
    if let Some(indexed) = &searcher.vector_source {
        if *indexed != source {
            worker.close();
            bail!("index vectors come from {indexed} but the query encoder is {source}");
        }
    }
    let prompted: Vec<String> =
        queries.iter().map(|query| if code { prompts::code_query(query) } else { prompts::query(query) }).collect();
    let vectors = worker.embed_texts(&prompted);
    worker.close();
    Ok(Some(vectors?))
}

fn open_searcher(args: &Arguments, code: bool) -> Result<Searcher> {
    let store = Store::open(&args.index_folder(), if code { store::CODE_FILE } else { store::MAIN_FILE })?;
    Searcher::new(store, true)
}

fn search(args: &Arguments) -> Result<()> {
    let mut query = args.positional.join(" ");
    if query.is_empty() {
        bail!("search for what?");
    }
    let code = code_query::text(&query);
    if let Some(code) = &code {
        query = code.clone();
    }
    if query.is_empty() {
        bail!("search the code for what?");
    }
    let mut searcher = open_searcher(args, code.is_some())?;
    let started = Instant::now();
    let vector = query_vectors(std::slice::from_ref(&query), &searcher.store, &searcher, code.is_some())?
        .and_then(|vectors| vectors.into_iter().next());
    let encoded = started.elapsed();
    let kinds = args.kinds()?;
    let hits = searcher.search(&query, vector.as_deref(), kinds.as_ref(), args.number("-n").unwrap_or(10),
                               args.z_gate(), args.word_weight(), false)?;
    let searched = started.elapsed() - encoded;
    if args.flag("--json") {
        let rows: Vec<serde_json::Value> = hits.iter().map(|hit| cli::hit_row(hit, false)).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if hits.is_empty() {
        println!("no good matches");
    }
    for (rank, hit) in hits.iter().enumerate() {
        let why: Vec<String> = [hit.z.map(|z| format!("z {z:.1}")), hit.keyword_rank.map(|rank| format!("words #{rank}"))]
            .into_iter()
            .flatten()
            .collect();
        let place = location(hit);
        println!("{}. {} {}{}   ({})", lpad(&(rank + 1).to_string(), 2), pad(hit.kind.as_str(), 10), file_name(&hit.path),
                 if place.is_empty() { String::new() } else { format!("  [{place}]") }, why.join(", "));
        println!("      {}", tilde(&parent_folder(&hit.path)));
        if let Some(excerpt) = hit.excerpt.as_ref().filter(|excerpt| !excerpt.is_empty()) {
            println!("      “{}”", text::prefix(excerpt, 160));
        }
    }
    println!("({} vectors · query encoder {:.2} s incl. start-up · search {:.0} ms)", searcher.vector_count(),
             encoded.as_secs_f64(), searched.as_secs_f64() * 1000.0);
    Ok(())
}

// MARK: Status

fn status(args: &Arguments) -> Result<()> {
    let store = args.store()?;
    let counts = store.counts()?;
    println!("index: {}", tilde(&store.file.to_string_lossy()));
    println!("vectors: {} in {} segments", number(counts.vectors), number(counts.segments));
    for kind in FileKind::ALL {
        let Some(states) = counts.by_kind_and_state.get(kind.as_str()) else { continue };
        let total: usize = states.values().sum();
        println!("  {} {}   {}", pad(kind.as_str(), 11), lpad(&number(total), 6), states_line(states));
    }
    if counts.unfinished > 0 {
        println!("long files still being read: {} ({} pages and chunks left)", number(counts.unfinished),
                 number(counts.left));
    }
    if let Some(fingerprint) = store.meta("fingerprint")? {
        println!("built with: {fingerprint}");
    }
    println!("this recall: llama.cpp {}, vectors {}", llama::build_tag(), llama::VECTOR_VERSION);
    println!("size: {}", bytes(Store::size_of(&store.file) as f64));
    Ok(())
}

// MARK: Eval

#[derive(Deserialize)]
struct EvalFile {
    base: String,
    queries: Vec<EvalQuery>,
}

#[derive(Deserialize)]
struct EvalQuery {
    q: String,
    /// Paths relative to `base`; "path@6-12" also requires the hit to start in [6, 12] s.
    expect: Vec<String>,
    tag: Option<String>,
    kind: Option<String>,
}

#[derive(Default, Clone, Copy)]
struct Row {
    n: usize,
    at1: usize,
    at5: usize,
    at10: usize,
    mrr: f64,
}

fn evaluate(args: &Arguments) -> Result<()> {
    let file = args.positional.first().ok_or_else(|| anyhow!("which eval file?"))?;
    let eval: EvalFile = serde_json::from_slice(&std::fs::read(paths::expand(file))?)?;
    let base = paths::expand(&eval.base).to_string_lossy().into_owned();
    // "code: …" queries search the code index (with its prompt), the others the main one.
    let is_code: Vec<bool> = eval.queries.iter().map(|query| code_query::text(&query.q).is_some()).collect();
    let texts: Vec<String> = eval.queries.iter().map(|query| code_query::text(&query.q).unwrap_or(query.q.clone())).collect();
    let mut searchers: HashMap<bool, Searcher> = HashMap::new();
    let mut vectors: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
    for code in is_code.iter().copied().collect::<HashSet<_>>() {
        let searcher = open_searcher(args, code)?;
        let indices: Vec<usize> = (0..texts.len()).filter(|index| is_code[*index] == code).collect();
        let queries: Vec<String> = indices.iter().map(|index| texts[*index].clone()).collect();
        if let Some(embedded) = query_vectors(&queries, &searcher.store, &searcher, code)? {
            for (index, vector) in indices.into_iter().zip(embedded) {
                vectors[index] = Some(vector);
            }
        }
        searchers.insert(code, searcher);
    }
    let depth = args.number("-n").unwrap_or(10);
    let mut by_tag: BTreeMap<String, Row> = BTreeMap::new();
    let (mut shown, mut listed, mut folded) = (0, 0, 0); // the app's weaker-matches fold, over all queries
    for (index, query) in eval.queries.iter().enumerate() {
        let kinds = query.kind.as_deref().and_then(FileKind::parse).map(|kind| HashSet::from([kind]));
        let searcher = searchers.get_mut(&is_code[index]).expect("opened");
        let hits = searcher.search(&texts[index], vectors[index].as_deref(), kinds.as_ref(), depth, args.z_gate(),
                                   args.word_weight(), false)?;
        let strong = searcher::strong_count(&hits, 0.5);
        shown += strong;
        listed += hits.len();
        let targets: Vec<(String, Option<(f64, f64)>)> = query
            .expect
            .iter()
            .map(|entry| {
                let (path, range) = entry.split_once('@').map_or((entry.as_str(), None), |(path, range)| (path, Some(range)));
                let range = range.and_then(|range| {
                    let bounds: Vec<f64> = range.split('-').filter_map(|bound| bound.parse().ok()).collect();
                    (bounds.len() == 2).then(|| (bounds[0], bounds[1]))
                });
                (format!("{base}/{path}"), range)
            })
            .collect();
        // A place to be at: a page or moment starts in the range; a stretch of code overlaps it (its lines or cells).
        let at = |hit: &SearchHit, (low, high): (f64, f64)| -> bool {
            let Some(loc) = hit.loc else { return false };
            if matches!(hit.segment, SegmentKind::Lines | SegmentKind::Cells) {
                return loc <= high && hit.loc_end.unwrap_or(loc) >= low;
            }
            (low..=high).contains(&loc)
        };
        let rank = hits
            .iter()
            .position(|hit| {
                targets.iter().any(|(path, range)| hit.path == *path && range.is_none_or(|range| at(hit, range)))
            })
            .map(|position| position + 1);
        if rank.is_some_and(|rank| rank > strong) {
            folded += 1;
        }
        let tag = query.tag.clone().unwrap_or_else(|| "all".into());
        for key in [tag.clone(), "ALL".to_string()].into_iter().collect::<HashSet<_>>() {
            let row = by_tag.entry(key).or_default();
            row.n += 1;
            if let Some(rank) = rank {
                row.at1 += usize::from(rank <= 1);
                row.at5 += usize::from(rank <= 5);
                row.at10 += usize::from(rank <= 10);
                row.mrr += 1.0 / rank as f64;
            }
        }
        let top = hits
            .first()
            .map(|hit| {
                let place = location(hit);
                file_name(&hit.path) + &if place.is_empty() { String::new() } else { format!(" [{place}]") }
            })
            .unwrap_or_else(|| "-".into());
        let rank_text = rank.map(|rank| lpad(&format!("#{rank}"), 4)).unwrap_or_else(|| "miss".into());
        println!("{rank_text}  {} {} top: {top}", pad(&tag, 11), pad(&format!("“{}”", query.q), 42));
    }
    println!();
    println!("{} {}  {}  {}  {}  {}", pad("tag", 12), lpad("n", 3), lpad("R@1", 5), lpad("R@5", 5), lpad("R@10", 5),
             lpad("MRR", 5));
    let mut rows: Vec<(&String, &Row)> = by_tag.iter().collect();
    rows.sort_by_key(|(tag, _)| if *tag == "ALL" { "~".to_string() } else { (*tag).clone() });
    for (tag, row) in rows {
        let n = row.n as f64;
        println!("{} {}  {:5.2}  {:5.2}  {:5.2}  {:5.2}", pad(tag, 12), lpad(&row.n.to_string(), 3), row.at1 as f64 / n,
                 row.at5 as f64 / n, row.at10 as f64 / n, row.mrr / n);
    }
    let count = eval.queries.len().max(1) as f64;
    println!("weaker-matches fold (below 0.5× the top score): {:.1} of {:.1} results shown per query; {folded} \
              expected {} folded", shown as f64 / count, listed as f64 / count,
             if folded == 1 { "answer" } else { "answers" });
    Ok(())
}

// MARK: Main

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = raw.first().filter(|command| !["-h", "--help", "help"].contains(&command.as_str())) else {
        println!("{}", usage());
        return;
    };
    let result = Arguments::parse(&raw[1..]).and_then(|args| match command.as_str() {
        "estimate" => estimate(&args).map(|_| 0),
        "index" => index(&args).map(|_| 0),
        "search" => search(&args).map(|_| 0),
        "devices" => { println!("{}", llama::diagnostics()); Ok(0) },
        "status" => status(&args).map(|_| 0),
        "eval" => evaluate(&args).map(|_| 0),
        "index-service" => cli::index_service::serve(&args),
        "worker" => Ok(cli::worker::serve(&args)),
        "serve" => Ok(cli::serve::serve(&args)),
        _ => {
            println!("{}", usage());
            Ok(2)
        }
    });
    match result {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    }
}
