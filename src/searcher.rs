//! Hybrid search: meaning (EmbeddingGemma's vectors) + words (FTS5).
//!
//! Meaning is the model's cosine similarity, put on one scale per query (`meaning`): every vector is scored anyway, so
//! the distribution comes for free, and only segments that stand out (`z_gate`) count as meaning matches. That also
//! gives an honest "nothing found". Words rescue what a vector can't hold exactly: codes, IDs and names.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::LazyLock;

use anyhow::Result;
use half::f16;
use rusqlite::{params, OptionalExtension};

use crate::code;
use crate::model::{FileKind, IndexOptions, Modality, SegmentKind};
use crate::store::{self, Info, Store};
use crate::text;
use crate::vectors::{VectorStorage, DIMENSION};

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub path: String,
    pub kind: FileKind,
    /// Fused rank score, for ordering only.
    pub score: f64,
    /// How far the best segment stands out for this query (`meaning`).
    pub z: Option<f64>,
    pub cosine: Option<f64>,
    /// 1-based rank among keyword matches.
    pub keyword_rank: Option<usize>,
    pub segment: SegmentKind,
    /// The segment the hit points at (its text is `Searcher::text_of_segment`).
    pub segment_id: i64,
    /// PDF page, or seconds into audio/video.
    pub loc: Option<f64>,
    pub loc_end: Option<f64>,
    pub excerpt: Option<String>,
    /// The file's modification date (seconds since 1970), and what indexing learned about it: "width", "height",
    /// "taken", "pages", "duration"… (see `indexer`).
    pub modified: f64,
    pub size: i64,
    pub info: Info,
    /// Code: the line to open at, in the stretch (`loc`…`loc_end`): where the name looked up is defined, or the first
    /// line with the query's words. None: the stretch's first line.
    pub focus: Option<f64>,
}

/// How much finding the query's words counts against meaning (`search`). Code's words count for less: they're
/// everywhere in code ("app", "running", "location"), and a stretch that shares a few of them beat the one that does
/// what the query says.
pub const WORD_WEIGHT: f64 = 2.0;
pub const CODE_WORD_WEIGHT: f64 = 1.0;

pub struct Searcher {
    pub store: Store,
    matrix: Option<VectorStorage>,
    row_file: Vec<i64>,
    row_segment: Vec<i64>,
    row_modality: Vec<Modality>,
    /// False for the vectors the later pass adds (`Indexer::finish_long_files`): scored, but they don't set the scale.
    row_sets_scale: Vec<bool>,
    file_kind: HashMap<i64, FileKind>,
    pub vector_source: Option<String>,
    /// False after `release_vectors()` (or `load_vectors: false`): keyword search still works, and the next search with
    /// a query vector loads them again.
    pub vectors_loaded: bool,
}

#[derive(Clone, Copy)]
struct Best {
    z: f64,
    cosine: f64,
    row: usize,
}

#[derive(Clone)]
struct WordMatch {
    file: i64,
    segment: i64,
    snippet: Option<String>,
}

impl Searcher {
    /// With `load_vectors: false` only keyword search is ready; vectors load on the first meaning search. The app uses
    /// that to stay small while idle: 20k vectors are ~30 MB.
    pub fn new(store: Store, load_vectors: bool) -> Result<Self> {
        let mut searcher = Self {
            store,
            matrix: None,
            row_file: Vec::new(),
            row_segment: Vec::new(),
            row_modality: Vec::new(),
            row_sets_scale: Vec::new(),
            file_kind: HashMap::new(),
            vector_source: None,
            vectors_loaded: false,
        };
        if load_vectors {
            searcher.reload()?;
        } else {
            searcher.reload_files()?;
        }
        Ok(searcher)
    }

    pub fn vector_count(&self) -> usize {
        self.row_file.len()
    }

    /// This is the code index's searcher: names are looked up as names.
    pub fn is_code(&self) -> bool {
        self.store.is_code()
    }

    /// Reads the files and all vectors again (after indexing added some).
    pub fn reload(&mut self) -> Result<()> {
        self.release_vectors();
        self.reload_files()?;
        let rows: i64 =
            self.store.db.query_row("SELECT COUNT(*) FROM segments WHERE vec IS NOT NULL", [], |row| row.get(0))?;
        let rows = rows as usize;
        if rows == 0 {
            self.vectors_loaded = true; // nothing to load
            return Ok(());
        }
        let mut matrix = VectorStorage::new(rows);
        let steps = IndexOptions::default();
        let mut statement = self.store.db.prepare(
            "SELECT s.id, s.file, s.modality, s.vec, s.kind, s.loc, f.kind, \
             CAST(json_extract(f.info, '$.pages') AS INTEGER) \
             FROM segments s JOIN files f ON f.id = s.file WHERE s.vec IS NOT NULL ORDER BY s.id",
        )?;
        let mut cursor = statement.query([])?;
        while let Some(row) = cursor.next()? {
            let blob = row.get_ref(3)?.as_blob()?;
            if blob.len() != DIMENSION * 2 || self.row_file.len() >= rows {
                continue;
            }
            matrix.halves.extend(blob.as_chunks::<2>().0.iter().map(|pair| f16::from_le_bytes(*pair)));
            self.row_segment.push(row.get(0)?);
            self.row_file.push(row.get(1)?);
            self.row_modality.push(Modality::parse(&row.get::<_, Option<String>>(2)?.unwrap_or_default()));
            // Past the first step of a long PDF (its pages) or document (its chunks).
            let kind: Option<String> = row.get(4)?;
            let place = row.get::<_, Option<f64>>(5)?.map_or(0, |loc| loc as i64);
            let file_kind: Option<String> = row.get(6)?;
            let pages = row.get::<_, Option<i64>>(7)?.unwrap_or(0);
            let chunk = kind.as_deref() == Some(SegmentKind::Chunk.as_str());
            let later = if file_kind.as_deref() == Some("pdf") {
                !chunk && pages > steps.big_pdf_pages as i64 && place > steps.pdf_step as i64
            } else {
                chunk && place > steps.doc_step as i64
            };
            self.row_sets_scale.push(!later);
        }
        matrix.rows = self.row_file.len();
        self.matrix = Some(matrix);
        self.vectors_loaded = true;
        Ok(())
    }

    /// Refresh keyword metadata now; load updated vectors only when semantic search needs them.
    pub fn invalidate(&mut self) -> Result<()> {
        self.release_vectors();
        self.reload_files()
    }

    /// Frees the vectors' memory; keyword search keeps working.
    pub fn release_vectors(&mut self) {
        self.matrix = None;
        self.row_file = Vec::new();
        self.row_segment = Vec::new();
        self.row_modality = Vec::new();
        self.row_sets_scale = Vec::new();
        self.vectors_loaded = false;
    }

    fn reload_files(&mut self) -> Result<()> {
        self.file_kind.clear();
        let mut statement = self.store.db.prepare("SELECT id, kind FROM files")?;
        let rows = statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, kind) = row?;
            if let Some(kind) = FileKind::parse(&kind) {
                self.file_kind.insert(id, kind);
            }
        }
        drop(statement);
        self.vector_source = self.store.meta("vector_source")?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn search(&mut self, text: &str, query_vector: Option<&[f32]>, kinds: Option<&HashSet<FileKind>>,
                  limit: usize, z_gate: f64, word_weight: Option<f64>, prefix_last: bool) -> Result<Vec<SearchHit>> {
        let is_code = self.is_code();
        let word_weight = word_weight.unwrap_or(if is_code { CODE_WORD_WEIGHT } else { WORD_WEIGHT });
        if query_vector.is_some() && !self.vectors_loaded {
            self.reload()?;
        }
        let file_kind = &self.file_kind;
        let allowed = |file: i64| match kinds {
            None => true,
            Some(kinds) => file_kind.get(&file).is_some_and(|kind| kinds.contains(kind)),
        };

        // Meaning: score every vector on this query's scale (`meaning`), keep each file's best segment.
        let mut best: HashMap<i64, Best> = HashMap::new();
        if let (Some(query), Some(matrix)) = (query_vector, &mut self.matrix) {
            if query.len() == DIMENSION && !self.row_file.is_empty() {
                let scores = matrix.score(query);
                let meaning = meaning(&scores, &self.row_modality, Some(&self.row_sets_scale));
                for row in 0..scores.len() {
                    let file = self.row_file[row];
                    if !allowed(file) {
                        continue;
                    }
                    let z = meaning[row];
                    if z > best.get(&file).map_or(f64::NEG_INFINITY, |best| best.z) {
                        best.insert(file, Best { z, cosine: f64::from(scores[row]), row });
                    }
                }
            }
        }

        // Words: FTS5 with Porter stemming over file names, folder names, OCR, page and document text. A name from code
        // ("quantize_row_q8_0_ref") is matched as one phrase, as it's written.
        let terms = terms(text);
        let identifier = if is_code { code::identifier(text) } else { None };
        let mut words: Vec<WordMatch> = Vec::new();
        let mut coverage: HashMap<i64, f64> = HashMap::new(); // share of the query's words found anywhere in the file
        let patterns = match &identifier {
            Some(identifier) => vec![code::phrase(identifier)],
            None => patterns(&terms, prefix_last),
        };
        if !patterns.is_empty() {
            let mut seen = HashSet::new();
            let mut statement = self.store.db.prepare(
                "SELECT s.file, s.id, snippet(fts, 1, '«', '»', '…', 12), fts.original FROM fts \
                 JOIN segments s ON s.id = fts.rowid \
                 WHERE fts MATCH ? ORDER BY bm25(fts, 3.0, 1.0) LIMIT 1000",
            )?;
            let mut cursor = statement.query([patterns.join(" OR ")])?;
            while let Some(row) = cursor.next()? {
                let file: i64 = row.get(0)?;
                if words.len() >= 100 || !allowed(file) || !seen.insert(file) {
                    continue;
                }
                // Text that normalizing changed is quoted as it's written (with its hamza, harakat, joiners).
                let original: Option<String> = row.get(3)?;
                let snippet = original
                    .and_then(|original| text::snippet(&original, &terms, 12))
                    .or(row.get::<_, Option<String>>(2)?);
                words.push(WordMatch { file, segment: row.get(1)?, snippet });
            }
            drop(cursor);
            drop(statement);
            if !words.is_empty() {
                let files: Vec<String> = words.iter().map(|word| word.file.to_string()).collect();
                let sql = format!(
                    "SELECT DISTINCT s.file FROM fts JOIN segments s ON s.id = fts.rowid \
                     WHERE fts MATCH ? AND s.file IN ({})",
                    files.join(",")
                );
                let mut statement = self.store.db.prepare(&sql)?;
                for pattern in &patterns {
                    let found = statement.query_map([pattern], |row| row.get::<_, i64>(0))?;
                    for file in found {
                        *coverage.entry(file?).or_insert(0.0) += 1.0 / patterns.len() as f64;
                    }
                }
            }
        }

        // A file qualifies by standing out in meaning, or by containing at least half of the query's words.
        // Score: meaning z + a bonus that grows with word coverage, + 1 for the kind the query asks for.
        // One common word can't beat a strong meaning match; all the words of an exact lookup usually do.
        let lookup = is_lookup(&terms) || identifier.is_some();
        let (meaning_weight, words_weight) = if lookup { (0.5, word_weight * 2.0) } else { (1.0, word_weight) };
        let hinted = kind_hints(text);
        let mut candidates: HashSet<i64> =
            best.iter().filter(|(_, best)| best.z >= z_gate).map(|(file, _)| *file).collect();
        for (file, share) in &coverage {
            if *share >= 0.5 {
                candidates.insert(*file);
            }
        }
        let mut scored: Vec<(i64, f64)> = candidates
            .into_iter()
            .map(|file| {
                let z = best.get(&file).map_or(0.0, |best| best.z).max(0.0);
                let mut score =
                    meaning_weight * z + words_weight * coverage.get(&file).copied().unwrap_or(0.0).powf(1.5);
                if file_kind.get(&file).is_some_and(|kind| hinted.contains(kind)) {
                    score += 1.0;
                }
                (file, score)
            })
            .collect();
        let compare = |a: &(i64, f64), b: &(i64, f64)| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0));
        if limit < scored.len() {
            scored.select_nth_unstable_by(limit, compare);
            scored.truncate(limit);
        }
        scored.sort_unstable_by(compare);

        let mut word_rank: HashMap<i64, (usize, WordMatch)> = HashMap::new();
        for (offset, word) in words.iter().enumerate() {
            word_rank.entry(word.file).or_insert((offset + 1, word.clone()));
        }
        let mut hits = Vec::new();
        for (file, score) in scored.into_iter().take(limit) {
            let rank = hits.len();
            let semantic = best.get(&file).copied();
            let keyword = word_rank.get(&file);
            // Point at the best meaning segment if it stands out; else at the segment the words matched.
            let use_meaning = semantic.is_some_and(|semantic| semantic.z >= z_gate || keyword.is_none());
            let segment = if use_meaning {
                semantic.map(|semantic| self.row_segment[semantic.row])
            } else {
                keyword.map(|(_, word)| word.segment)
            };
            let Some(mut segment_id) = segment else { continue };
            let mut focus = None;
            if is_code {
                // Where a name is defined is looked for in the first 10 results only: a name used all over a big repo
                // (ggml_tensor) is in hundreds of stretches of every file that has it.
                let words_here = keyword.is_some_and(|(_, word)| word.segment == segment_id) || identifier.is_some();
                (segment_id, focus) = self.code_place(file, segment_id,
                    if rank < 10 { identifier.as_deref() } else { None }, text, &terms, words_here)?;
            }
            let (path, kind, modified, size, info) = self.store.db.query_row(
                "SELECT path, kind, mtime, size, info FROM files WHERE id = ?",
                [file],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        FileKind::parse(&row.get::<_, String>(1)?).unwrap_or(FileKind::Doc),
                        row.get::<_, f64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<String>>(4)?.and_then(|json| store::parse_info(&json)).unwrap_or_default(),
                    ))
                },
            )?;
            let (segment_kind, loc, loc_end, stored_excerpt) = self.store.db.query_row(
                "SELECT kind, loc, loc_end, excerpt FROM segments WHERE id = ?",
                [segment_id],
                |row| {
                    Ok((
                        SegmentKind::parse(&row.get::<_, String>(0)?).unwrap_or(SegmentKind::Name),
                        row.get::<_, Option<f64>>(1)?,
                        row.get::<_, Option<f64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )?;
            // Quote the matched words only when they come from the segment we point at, so a PDF hit on page 5 never
            // shows page 2's words. A segment with no place and no text of its own (a screenshot's image vector) can
            // borrow them from its file's OCR text, which describes the same picture.
            let mut snippet = keyword
                .filter(|(_, word)| word.segment == segment_id)
                .and_then(|(_, word)| word.snippet.as_deref())
                .filter(|snippet| snippet.contains('«'))
                .map(|snippet| text::excerpt(snippet, 400));
            if snippet.is_none() && stored_excerpt.is_none() && loc.is_none() {
                if let Some((_, matched)) = keyword {
                    if let Some(found) = matched.snippet.as_deref().filter(|snippet| snippet.contains('«')) {
                        let matched_kind: Option<String> = self
                            .store
                            .db
                            .query_row("SELECT kind FROM segments WHERE id = ?", [matched.segment], |row| row.get(0))
                            .optional()?;
                        if matched_kind.as_deref() != Some(SegmentKind::Name.as_str()) {
                            snippet = Some(text::excerpt(found, 400));
                        }
                    }
                }
            }
            hits.push(SearchHit {
                path,
                kind,
                score,
                z: semantic.map(|semantic| semantic.z),
                cosine: semantic.map(|semantic| semantic.cosine),
                keyword_rank: keyword.map(|(rank, _)| *rank),
                segment: segment_kind,
                segment_id,
                loc,
                loc_end,
                excerpt: snippet.or(stored_excerpt),
                modified,
                size,
                info,
                focus,
            });
        }
        Ok(hits)
    }

    /// Code: the stretch to point at, and the line in it to open at. A name looked up points at the stretch that
    /// defines it, when one does (not the ones that call it). Otherwise the line is the first with the query's words,
    /// when they matched in this stretch; a match by meaning alone opens at the stretch's first line.
    fn code_place(&self, file: i64, segment: i64, identifier: Option<&[String]>, query: &str, terms: &[String],
                  words_here: bool) -> Result<(i64, Option<f64>)> {
        let name = query.trim().replace("()", "");
        if let Some(identifier) = identifier {
            let mut best: Option<(i64, usize, u8)> = None;
            let mut statement = self.store.db.prepare(
                "SELECT s.id, s.loc, coalesce(fts.original, fts.body) FROM fts JOIN segments s ON s.id = fts.rowid \
                 WHERE fts MATCH ? AND s.file = ? ORDER BY s.loc LIMIT 300",
            )?;
            let mut cursor = statement.query(params![code::phrase(identifier), file])?;
            while let Some(row) = cursor.next()? {
                if best.map_or(0, |best| best.2) >= 2 {
                    break;
                }
                let Some(text) = row.get::<_, Option<String>>(2)? else { continue };
                let first = row.get::<_, Option<f64>>(1)?.unwrap_or(0.0) as usize;
                if let Some((line, strength)) = code::definition(&name, &text, first) {
                    if strength > best.map_or(0, |best| best.2) {
                        best = Some((row.get(0)?, line, strength));
                    }
                }
            }
            if let Some((segment, line, _)) = best {
                return Ok((segment, Some(line as f64)));
            }
        }
        if !(words_here || identifier.is_some()) {
            return Ok((segment, None));
        }
        let Some(text) = self.text_of_segment(segment)? else { return Ok((segment, None)) };
        let first: Option<f64> = self
            .store
            .db
            .query_row("SELECT loc FROM segments WHERE id = ?", [segment], |row| row.get(0))
            .optional()?
            .flatten();
        let words = if identifier.is_none() { terms.to_vec() } else { vec![name.to_lowercase()] };
        Ok((segment, code::first_line(&words, &text, first.unwrap_or(1.0) as usize).map(|line| line as f64)))
    }

    /// Source path for checking a passage against the desktop's current folder selection.
    pub fn path_of_segment(&self, id: i64) -> Result<Option<String>> {
        Ok(self.store.db.query_row(
            "SELECT f.path FROM segments s JOIN files f ON f.id = s.file WHERE s.id = ?",
            [id], |row| row.get(0)).optional()?)
    }

    /// The text of a page, a document's passage or a screenshot's OCR, as it's written: the preview shows the part of a
    /// document that matched.
    pub fn text_of_segment(&self, id: i64) -> Result<Option<String>> {
        Ok(self
            .store
            .db
            .query_row("SELECT coalesce(original, body) FROM fts WHERE rowid = ?", [id], |row| row.get(0))
            .optional()?
            .flatten())
    }
}

/// Codes and IDs ("K7Q2LM", "TRX50") and long numbers ("4815162342": an order, a phone number) mean nothing to an
/// embedding but everything to keyword search: a query made of them weighs words double and meaning half.
pub fn is_lookup(terms: &[String]) -> bool {
    !terms.is_empty()
        && terms.iter().all(|term| {
            term.chars().any(char::is_numeric)
                && (term.chars().any(char::is_alphabetic) || text::length_chars(term) >= 4)
        })
}

/// How many of `hits` (best first) are strong: scoring at least `ratio` × the top hit. The rest pass the gates but
/// trail far behind, and the app folds them under "weaker matches".
pub fn strong_count(hits: &[SearchHit], ratio: f64) -> usize {
    match hits.first() {
        Some(top) if top.score > 0.0 => hits.iter().take_while(|hit| hit.score >= top.score * ratio).count(),
        _ => hits.len(),
    }
}

/// This query's meaning scores: each vector's cosine similarity minus the average of its modality (text, image and
/// audio vectors can sit at slightly different levels for the same query), in standard deviations of all of them
/// together. Within a modality that's the model's own order.
///
/// The scale (the averages and the deviation) comes from the vectors in `sets_scale` (all by default): `search` leaves
/// out the later pages of long files, which are scored on it but don't move it.
pub fn meaning(scores: &[f32], modality: &[Modality], sets_scale: Option<&[bool]>) -> Vec<f64> {
    let counts = |row: usize| sets_scale.is_none_or(|sets| sets[row]);
    let means = modality_means(scores, modality, sets_scale);
    let centered: Vec<f64> = scores
        .iter()
        .zip(modality)
        .map(|(score, modality)| f64::from(*score) - means.get(modality).copied().unwrap_or(0.0))
        .collect();
    let (mut count, mut sum) = (0.0, 0.0);
    for (row, value) in centered.iter().enumerate() {
        if counts(row) {
            count += 1.0;
            sum += value;
        }
    }
    let mean = sum / f64::max(count, 1.0);
    let mut squares = 0.0;
    for (row, value) in centered.iter().enumerate() {
        if counts(row) {
            squares += (value - mean) * (value - mean);
        }
    }
    let deviation = (squares / f64::max(count, 1.0)).sqrt().max(0.02);
    centered.iter().map(|value| (value - mean) / deviation).collect()
}

/// The average score of each modality for this query. Small groups (< 20 vectors) take the average of all.
pub fn modality_means(scores: &[f32], modality: &[Modality], sets_scale: Option<&[bool]>) -> HashMap<Modality, f64> {
    let mut sum: BTreeMap<Modality, f64> = BTreeMap::new();
    let mut count: BTreeMap<Modality, usize> = BTreeMap::new();
    for row in 0..scores.len() {
        if sets_scale.is_none_or(|sets| sets[row]) {
            *sum.entry(modality[row]).or_insert(0.0) += f64::from(scores[row]);
            *count.entry(modality[row]).or_insert(0) += 1;
        }
    }
    let all = sum.values().sum::<f64>() / count.values().sum::<usize>().max(1) as f64;
    Modality::ALL
        .into_iter()
        .map(|modality| match count.get(&modality) {
            Some(&n) if n >= 20 => (modality, sum[&modality] / n as f64),
            _ => (modality, all),
        })
        .collect()
}

static STOPWORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "a", "an", "the", "of", "in", "on", "at", "to", "for", "with", "and", "or", "my", "me", "that", "this",
        "these", "those", "from", "about", "where", "what", "which", "who", "when", "is", "was", "are", "were", "be",
        "it", "its", "some", "any", "find", "show", "file", "files", "photo", "photos", "picture", "image", "images",
        "screenshot", "screenshots", "video", "videos", "pdf", "document", "documents", "recording",
        // Bengali and Arabic (normalized, as `terms` sees them)
        "এবং", "ও", "এই", "সেই", "একটি", "আমার", "থেকে", "জন্য", "সাথে", "মধ্যে", "কোথায়", "কোন", "কী", "কি", "যে",
        "ছবি", "স্ক্রিনশট", "ভিডিও", "ফাইল", "পিডিএফ",
        "في", "من", "على", "الى", "عن", "مع", "هذا", "هذه", "ذلك", "التي", "الذي", "اين", "ما",
        "صورة", "صور", "لقطة", "شاشة", "فيديو", "ملف", "مستند",
    ]
    .into_iter()
    .collect()
});

fn words_of(text: &str) -> Vec<String> {
    text::normalized(text)
        .to_lowercase()
        .split(|ch: char| !text::is_word_char(ch))
        .map(String::from)
        .collect()
}

/// The content words of a query, normalized as keyword search keeps its text. Kind words ("screenshot", "video") and
/// stopwords are dropped: they describe what to look for, not what's in it. Words of two letters or more count; a
/// Bengali letter with its vowel sign ("মা") is two.
pub fn terms(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    words_of(text)
        .into_iter()
        .filter(|word| word.chars().count() >= 2 && !STOPWORDS.contains(word.as_str()) && seen.insert(word.clone()))
        .collect()
}

/// FTS5 patterns for the query's words: quoted, and as prefixes for the last word while it's being typed and for words
/// of languages that attach endings (`text::inflects`).
pub fn patterns(terms: &[String], prefix_last: bool) -> Vec<String> {
    terms
        .iter()
        .enumerate()
        .map(|(index, term)| {
            let prefix = (prefix_last && index == terms.len() - 1) || text::inflects(term);
            format!("\"{term}\"{}", if prefix { "*" } else { "" })
        })
        .collect()
}

/// The query's words as an FTS5 OR-query.
pub fn fts_query(text: &str, prefix_last: bool) -> Option<String> {
    let words = terms(text);
    (!words.is_empty()).then(|| patterns(&words, prefix_last).join(" OR "))
}

static KIND_WORDS: LazyLock<Vec<(&'static str, FileKind)>> = LazyLock::new(|| {
    use FileKind::*;
    vec![
        ("screenshot", Screenshot), ("screenshots", Screenshot), ("photo", Image), ("photos", Image),
        ("picture", Image), ("pictures", Image), ("image", Image), ("images", Image), ("pdf", Pdf), ("pdfs", Pdf),
        ("paper", Pdf), ("papers", Pdf), ("document", Doc), ("documents", Doc), ("note", Doc), ("notes", Doc),
        ("video", Video), ("videos", Video), ("clip", Video), ("movie", Video), ("audio", Audio), ("song", Audio),
        ("podcast", Audio), ("voice", Audio), ("sound", Audio),
        ("স্ক্রিনশট", Screenshot), ("ছবি", Image), ("পিডিএফ", Pdf), ("নোট", Doc), ("ভিডিও", Video), ("অডিও", Audio),
        ("গান", Audio), ("রেকর্ডিং", Audio),
        ("لقطة", Screenshot), ("صورة", Image), ("صور", Image), ("مستند", Doc), ("ملاحظات", Doc), ("فيديو", Video),
        ("مقطع", Video), ("صوت", Audio), ("تسجيل", Audio), ("اغنية", Audio),
    ]
});

/// Kinds the query asks for ("…in a video", "screenshot of…", "ভিডিওতে"): a soft boost, not a filter. Bengali
/// attaches case endings, so a Bengali kind word also counts at the start of a longer word.
pub fn kind_hints(text: &str) -> HashSet<FileKind> {
    words_of(text)
        .into_iter()
        .filter_map(|word| {
            KIND_WORDS.iter().find(|(key, _)| *key == word).map(|(_, kind)| *kind).or_else(|| {
                KIND_WORDS
                    .iter()
                    .find(|(key, _)| text::inflects(key) && key.chars().count() >= 3 && word.starts_with(key))
                    .map(|(_, kind)| *kind)
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_drop_kind_words_and_stopwords() {
        assert_eq!(terms("Find the screenshot of my Tokyo itinerary"), ["tokyo", "itinerary"]);
        assert_eq!(terms("invoice INV-2024-0042"), ["invoice", "inv", "2024", "0042"]);
    }

    #[test]
    fn lookups_are_codes_and_long_numbers() {
        assert!(is_lookup(&terms("K7Q2LM")));
        assert!(is_lookup(&terms("4815162342")));
        assert!(!is_lookup(&terms("meeting 42")));
        assert!(!is_lookup(&[]));
    }

    #[test]
    fn kind_hints_find_the_kind_asked_for() {
        assert_eq!(kind_hints("budget talk in a video"), HashSet::from([FileKind::Video]));
        assert!(kind_hints("budget").is_empty());
    }

    #[test]
    fn patterns_prefix_the_last_word_while_typing() {
        let words = terms("quarterly rev");
        assert_eq!(patterns(&words, true), ["\"quarterly\"", "\"rev\"*"]);
        assert_eq!(patterns(&words, false), ["\"quarterly\"", "\"rev\""]);
    }

    #[test]
    fn meaning_centers_each_modality() {
        let modality: Vec<Modality> = (0..40).map(|i| if i < 20 { Modality::Text } else { Modality::Image }).collect();
        let scores: Vec<f32> = (0..40).map(|i| if i < 20 { 0.3 + i as f32 * 0.001 } else { 0.1 + i as f32 * 0.001 }).collect();
        let z = meaning(&scores, &modality, None);
        // Each modality's top vector stands out the same, though images sit lower.
        assert!((z[19] - z[39]).abs() < 1e-4, "{} {}", z[19], z[39]);
        assert!(z[19] > z[0]);
    }

    #[test]
    fn fts_query_drops_stopwords_and_kind_words() {
        assert_eq!(fts_query("the screenshot of my payment error", false).as_deref(), Some("\"payment\" OR \"error\""));
        assert_eq!(fts_query("zebr", true).as_deref(), Some("\"zebr\"*"));
        assert_eq!(fts_query("a screenshot", false), None);
    }

    #[test]
    fn query_words_of_languages_with_endings_are_prefixes() {
        assert_eq!(fts_query("কুমির", false).as_deref(), Some("\"কুমির\"*"));
        assert_eq!(fts_query("مدرسة الأطفال", false).as_deref(), Some("\"مدرسة\"* OR \"الاطفال\"*"));
        assert_eq!(fts_query("মা", false).as_deref(), Some("\"মা\"*")); // a letter and its vowel sign: two, so it counts
        assert_eq!(fts_query("payment error", false).as_deref(), Some("\"payment\" OR \"error\""));
        assert_eq!(kind_hints("জেব্রার ভিডিওতে"), HashSet::from([FileKind::Video]));
        assert_eq!(kind_hints("صورة حمار وحشي"), HashSet::from([FileKind::Image]));
    }

    #[test]
    fn codes_and_long_numbers_are_lookups() {
        let lookup = |list: &[&str]| is_lookup(&list.iter().map(|term| term.to_string()).collect::<Vec<_>>());
        assert!(lookup(&["k7q2lm"]));
        assert!(lookup(&["trx50"]));
        assert!(lookup(&["4815162342"]));
        assert!(lookup(&["2041"]));
        assert!(!lookup(&["417"])); // too short to be an ID
        assert!(!lookup(&["inv", "2041"])); // a word in it
        assert!(!lookup(&[]));
    }

    #[test]
    fn weaker_matches_fold_below_half_the_top_score() {
        let hit = |score: f64| SearchHit {
            path: format!("/{score}"),
            kind: FileKind::Image,
            score,
            z: None,
            cosine: None,
            keyword_rank: None,
            segment: SegmentKind::Image,
            segment_id: 1,
            loc: None,
            loc_end: None,
            excerpt: None,
            modified: 0.0,
            size: 0,
            info: Info::new(),
            focus: None,
        };
        assert_eq!(strong_count(&[hit(6.0), hit(4.1), hit(3.0), hit(2.9), hit(0.7)], 0.5), 3);
        assert_eq!(strong_count(&[hit(0.71)], 0.5), 1);
        assert_eq!(strong_count(&[], 0.5), 0);
    }

    #[test]
    fn meaning_takes_out_each_modalitys_level_on_one_scale() {
        // Text scores sit higher than image scores here: that level comes out. The spread is shared, so a modality
        // whose scores are bunched up doesn't get its best ones inflated: 0.01 above average beats 0.002 above, 5:1.
        let text: Vec<f32> = (0..30).map(|i| 0.70 + (i % 3) as f32 * 0.01).collect();
        let image: Vec<f32> = (0..30).map(|i| 0.55 + (i % 3) as f32 * 0.002).collect();
        let scores = [text, image].concat();
        let modality = [vec![Modality::Text; 30], vec![Modality::Image; 30]].concat();
        let z = meaning(&scores, &modality, None);
        assert!(z[1].abs() < 1e-4 && z[31].abs() < 1e-4); // 0.71 and 0.552: each at its modality's average
        assert!((z[2] / z[32] - 5.0).abs() < 0.01);
        let means = modality_means(&scores, &modality, None);
        assert!((means[&Modality::Text] - 0.71).abs() < 0.001 && (means[&Modality::Image] - 0.552).abs() < 0.001);
        assert!((means[&Modality::Audio] - 0.631).abs() < 0.001); // no audio vectors: the average of all
    }

    #[test]
    fn later_pages_are_scored_without_moving_the_scale() {
        // A long book's later pages, far from the query, would pull the text average down and lift all text.
        let scores: Vec<f32> = (0..30)
            .map(|i| 0.70 + (i % 3) as f32 * 0.01)
            .chain((0..30).map(|i| 0.55 + (i % 3) as f32 * 0.002))
            .collect();
        let modality = [vec![Modality::Text; 30], vec![Modality::Image; 30]].concat();
        let mut book: Vec<f32> = (0..400).map(|i| 0.60 + (i % 5) as f32 * 0.004).collect();
        book.push(0.80); // one page that matches
        let z = meaning(&scores, &modality, None);
        let sets_scale = [vec![true; 60], vec![false; 401]].concat();
        let with_book = meaning(
            &[scores, book.clone()].concat(),
            &[modality, vec![Modality::Text; book.len()]].concat(),
            Some(&sets_scale),
        );
        for row in 0..60 {
            assert!((z[row] - with_book[row]).abs() < 1e-9);
        }
        // The matching page stands out on the same scale.
        assert!(*with_book.last().unwrap() > z.iter().copied().fold(f64::NEG_INFINITY, f64::max));
    }
}
