//! Recall's engine: finds the files in the chosen folders, reads them (text, pages, pictures, sound, video, code),
//! embeds them with EmbeddingGemma 2 on llama.cpp, keeps them in one SQLite index, and searches it by meaning and by
//! words.

pub mod code;
pub mod crawler;
pub mod docs;
pub mod embed;
pub mod estimator;
pub mod folders;
pub mod image;
pub mod indexer;
pub mod llama;
pub mod media;
pub mod model;
pub mod model_files;
pub mod ocr;
pub mod paths;
pub mod pdf;
pub mod rules;
pub mod searcher;
pub mod store;
pub mod text;
pub mod tools;
pub mod vectors;

pub mod shared;
