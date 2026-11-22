//! The vectors this build makes for fixed inputs, against the ones `llama::VECTOR_VERSION` stands for
//! (reference/vectors.json). Indexes record that version, not the llama.cpp build, so every llama.cpp update must pass
//! this before it ships. If it fails, keep the old llama.cpp, or bump `VECTOR_VERSION` (every index then embeds again)
//! and write the reference anew: `RECALL_WRITE_REFERENCE=1 cargo test --test reference_vectors`.
//!
//! The inputs go in as the indexer hands them over: texts with the app's prompts, a screenshot as PNG (280 tokens), a
//! photo as JPEG (140, and 70 as a video frame) and a 16 kHz WAV, all read by llama.cpp's own decoders. Needs the
//! model files; without them the test says it was skipped (or fails, with `RECALL_REQUIRE_MODELS=1`).

mod common;
use std::collections::BTreeMap;
use std::path::PathBuf;

use recall::embed::Embedder;
use recall::llama::{self, LlamaEmbedder, LlamaModels};
use recall::model::prompts;

/// Each vector's cosine with its reference must stay at least this: runtimes that agree (the reference and llama.cpp
/// on any backend) measure ≥ 0.999, and less is a change people would see in their results.
const AGREEMENT: f32 = 0.999;

fn folder() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/reference")
}

fn texts() -> Vec<(&'static str, String)> {
    let long: Vec<String> = (1..=60)
        .map(|n| {
            format!("Note {n}: the review moved to room {} because the projector in the main hall failed again, \
                     and the team agreed to send the slides the night before.", n % 7 + 2)
        })
        .collect();
    vec![
        ("query", prompts::query("payment declined error")),
        ("query-bn", prompts::query("ইলিশ মাছ রান্না")),
        ("query-ar", prompts::query("متى تغلق المكتبة")),
        ("document", prompts::document(Some("Tokyo itinerary"), "Flight to Tokyo Haneda on November 3. Then the train \
            to Kamakura to see the Great Buddha, and back for dinner in Shibuya.")),
        // About 1,800 tokens: past the sliding attention window, so the global layers count as well.
        ("document-long", prompts::document(None, &long.join(" "))),
    ]
}

fn vectors(models: &LlamaModels) -> BTreeMap<String, Vec<f32>> {
    let log = std::env::temp_dir().join("recall-reference-vectors.log");
    let mut embedder = LlamaEmbedder::new(models, false, Some(&log)).expect("loads");
    let mut made = BTreeMap::new();
    let inputs = texts();
    let texts: Vec<String> = inputs.iter().map(|(_, text)| text.clone()).collect();
    for ((name, _), vector) in inputs.iter().zip(embedder.embed_texts(&texts).expect("texts")) {
        made.insert(name.to_string(), vector);
    }
    for (name, file, budget) in [("screen-280", "screen.png", 280), ("photo-140", "photo.jpg", 140), ("frame-70", "photo.jpg", 70)] {
        let path = folder().join(file).to_string_lossy().into_owned();
        let embedding = embedder.embed_images(&[path], budget).expect("images").remove(0);
        made.insert(name.into(), embedding.vector.unwrap_or_else(|| panic!("{file}: {:?}", embedding.error)));
    }
    let path = folder().join("speech.wav").to_string_lossy().into_owned();
    let speech = embedder.embed_audio(&[path]).expect("audio").remove(0);
    made.insert("speech".into(), speech.vector.unwrap_or_else(|| panic!("speech.wav: {:?}", speech.error)));
    embedder.close();
    made
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[test]
fn vectors_match_the_reference() {
    // The model files are 865 MB and CI does not download them, so without them this is skipped, in plain sight.
    // RECALL_REQUIRE_MODELS=1 turns the skip into a failure, for a release check that must have compared the vectors.
    let required = std::env::var("RECALL_REQUIRE_MODELS").as_deref() == Ok("1");
    let Ok(models) = LlamaModels::locate() else {
        return common::skip("vectors_match_the_reference", "the EmbeddingGemma text model", required);
    };
    if models.media.is_none() {
        return common::skip("vectors_match_the_reference", "the EmbeddingGemma image/audio encoder", required);
    }
    let made = vectors(&models);
    let path = folder().join("vectors.json");
    let stored: Option<serde_json::Value> =
        std::fs::read(&path).ok().and_then(|data| serde_json::from_slice(&data).ok());
    let stored_vectors = |stored: &serde_json::Value| -> BTreeMap<String, Vec<f32>> {
        serde_json::from_value(stored["vectors"].clone()).expect("vectors")
    };
    if std::env::var("RECALL_WRITE_REFERENCE").as_deref() == Ok("1") {
        // A new reference for the same version would let indexes mix old and new vectors.
        if let Some(stored) = &stored {
            if stored["vector_version"] == llama::VECTOR_VERSION {
                let reference = stored_vectors(stored);
                for (name, vector) in &made {
                    if let Some(expected) = reference.get(name) {
                        let cosine = cosine(vector, expected);
                        assert!(cosine >= AGREEMENT, "{name} moved to cosine {cosine}: bump VECTOR_VERSION first");
                    }
                }
            }
        }
        let reference = serde_json::json!({
            "made_with": format!("llama.cpp {}", llama::build_tag()),
            "vector_version": llama::VECTOR_VERSION,
            "vectors": made,
        });
        std::fs::write(&path, reference.to_string()).expect("writes");
        return;
    }
    let stored = stored.expect("no reference/vectors.json: write it with RECALL_WRITE_REFERENCE=1");
    assert_eq!(stored["vector_version"], llama::VECTOR_VERSION, "the reference is for other vectors: write it anew");
    let reference = stored_vectors(&stored);
    assert_eq!(made.keys().collect::<Vec<_>>(), reference.keys().collect::<Vec<_>>());
    for (name, vector) in &made {
        let cosine = cosine(vector, &reference[name]);
        assert!(cosine >= AGREEMENT, "{name}: cosine {cosine} with {}. llama.cpp {} makes other vectors: keep the old \
            llama.cpp, or bump VECTOR_VERSION", stored["made_with"], llama::build_tag());
        eprintln!("{name}: {cosine:.5}");
    }
}
