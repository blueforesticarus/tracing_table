// Storage and packing data structures used by the recording subscriber.
pub mod blockbuffer;
pub mod dataframe;
pub mod dyntuple;

// Strings live in per-archetype buffers, shared by consecutive message chunks.
pub mod smallbox;
pub mod stringbuffer;

// The experimental smartstring implementation remains disabled.
// pub mod smartstring;

#[derive(Debug, Clone, Copy, Default)]
pub struct CompressionStats {
    pub data: usize,       // this is the size of the actual data before compression
    pub compressed: usize, // this this the space taken up by data, excluding metadata used to store it
    pub overhead: usize, // this is the space taken up by the metadata used to store the compressed data
    pub overalloc: usize, // this is the space taken up by empty allocated space
}

impl CompressionStats {
    /// Fraction of row bytes removed by compression (excluding overhead and
    /// unused capacity). Empty storage has no savings; multiply by 100 for UI.
    pub fn ratio(&self) -> f64 {
        if self.data == 0 {
            0.0
        } else {
            1.0 - (self.compressed + self.overalloc + self.overhead) as f64 / self.data as f64
        }
    }
    pub fn raw_ratio(&self) -> f64 {
        if self.data == 0 {
            0.0
        } else {
            1.0 - self.compressed as f64 / self.data as f64
        }
    }
}

impl std::ops::Add for CompressionStats {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        CompressionStats {
            data: self.data + rhs.data,
            compressed: self.compressed + rhs.compressed,
            overhead: self.overhead + rhs.overhead,
            overalloc: self.overalloc + rhs.overalloc,
        }
    }
}
