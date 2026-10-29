//! Vectors are stored as 768 × float16 (1.5 KB each), kept that way in memory too, and scored as float32.

use half::{f16, slice::HalfFloatSliceExt};

pub const DIMENSION: usize = 768;

/// A vector as stored: float16, little-endian.
pub fn half(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 2);
    for value in vector {
        out.extend_from_slice(&f16::from_f32(*value).to_le_bytes());
    }
    out
}

/// The float16 rows of a block turned into float32.
pub fn floats(halves: &[f16], out: &mut [f32]) {
    halves.convert_to_f32_slice(out);
}

/// `scores[r] = matrix[r] · vector` for rows of `DIMENSION` floats.
pub fn multiply(matrix: &[f32], vector: &[f32], scores: &mut [f32]) {
    for (row, score) in matrix.as_chunks::<DIMENSION>().0.iter().zip(scores.iter_mut()) {
        // Eight sums side by side: the compiler turns this into SIMD.
        let mut sums = [0f32; 8];
        for (a, b) in row.as_chunks::<8>().0.iter().zip(vector.as_chunks::<8>().0.iter()) {
            for lane in 0..8 {
                sums[lane] += a[lane] * b[lane];
            }
        }
        *score = sums.iter().sum();
    }
}

/// The vectors search scores, as stored (float16): half the memory of float32 (70,000 vectors: 108 MB instead of
/// 215), for a couple of milliseconds more a query.
pub struct VectorStorage {
    pub rows: usize,
    pub halves: Vec<f16>,
    scratch: Vec<f32>,
}

impl VectorStorage {
    pub const BLOCK_ROWS: usize = 1024;

    pub fn new(rows: usize) -> Self {
        Self { rows, halves: Vec::with_capacity(rows * DIMENSION), scratch: Vec::new() }
    }

    /// Cosine similarity of `query` with every row (all are L2-normalized), a block of rows at a time.
    pub fn score(&mut self, query: &[f32]) -> Vec<f32> {
        let rows = self.halves.len() / DIMENSION;
        let mut scores = vec![0f32; rows];
        self.scratch.resize(Self::BLOCK_ROWS.min(rows) * DIMENSION, 0.0);
        let mut row = 0;
        while row < rows {
            let count = Self::BLOCK_ROWS.min(rows - row);
            let block = &mut self.scratch[..count * DIMENSION];
            floats(&self.halves[row * DIMENSION..(row + count) * DIMENSION], block);
            multiply(block, query, &mut scores[row..row + count]);
            row += count;
        }
        scores
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halves_round_trip_and_score_across_blocks() {
        let rows = VectorStorage::BLOCK_ROWS + 37;
        let mut storage = VectorStorage::new(rows);
        let mut target = vec![0f32; DIMENSION];
        for row in 0..rows {
            let mut vector = vec![0f32; DIMENSION];
            vector[row % DIMENSION] = 1.0;
            if row == rows - 5 {
                target = vector.clone();
            }
            for pair in half(&vector).as_chunks::<2>().0.iter() {
                storage.halves.push(f16::from_le_bytes(*pair));
            }
        }
        let scores = storage.score(&target);
        assert_eq!(scores.len(), rows);
        assert!((scores[rows - 5] - 1.0).abs() < 0.002);
    }
}
