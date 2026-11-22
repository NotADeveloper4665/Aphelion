//! Bounded Reed-Solomon frame-shard encoding.
//!
//! This emits generic data/parity shards. Moonlight packet headers and block
//! metadata are a separate adapter stage; callers must not send these shards
//! without that metadata and the negotiated per-client transport.

use reed_solomon_erasure::galois_8::ReedSolomon;

pub const MAX_SHARDS_PER_BLOCK: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FecError {
    EmptyPayload,
    ZeroShardSize,
    PercentageOutOfRange(u8),
    TooManyDataShards(usize),
    TooManyTotalShards { data: usize, parity: usize },
    InvalidParameters,
    EncodeFailed,
}

impl std::fmt::Display for FecError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPayload => formatter.write_str("FEC payload must not be empty"),
            Self::ZeroShardSize => formatter.write_str("FEC shard size must be non-zero"),
            Self::PercentageOutOfRange(value) => write!(formatter, "FEC percentage {value} exceeds 100"),
            Self::TooManyDataShards(value) => write!(formatter, "FEC block requires {value} data shards; maximum is {MAX_SHARDS_PER_BLOCK}"),
            Self::TooManyTotalShards { data, parity } => write!(formatter, "FEC block requires {data} data plus {parity} parity shards; maximum total is {MAX_SHARDS_PER_BLOCK}"),
            Self::InvalidParameters => formatter.write_str("invalid Reed-Solomon shard parameters"),
            Self::EncodeFailed => formatter.write_str("Reed-Solomon parity encoding failed"),
        }
    }
}

impl std::error::Error for FecError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FecBlock {
    pub original_payload_len: usize,
    pub data_shards: usize,
    pub parity_shards: usize,
    /// Data shards followed by parity shards; all have equal `shard_size`.
    pub shards: Vec<Vec<u8>>,
}

impl FecBlock {
    /// Encode one bounded block. The payload must already include the media
    /// and per-packet protocol headers covered by FEC. Encryption is applied
    /// to the resulting individual shards by the caller.
    pub fn encode(
        payload: &[u8],
        shard_size: usize,
        fec_percent: u8,
        minimum_parity_shards: usize,
    ) -> Result<Self, FecError> {
        if payload.is_empty() { return Err(FecError::EmptyPayload); }
        if shard_size == 0 { return Err(FecError::ZeroShardSize); }
        if fec_percent > 100 { return Err(FecError::PercentageOutOfRange(fec_percent)); }
        let data_shards = payload.len().checked_add(shard_size - 1)
            .ok_or(FecError::InvalidParameters)? / shard_size;
        if data_shards > MAX_SHARDS_PER_BLOCK { return Err(FecError::TooManyDataShards(data_shards)); }
        let percentage_parity = data_shards.checked_mul(fec_percent as usize)
            .and_then(|count| count.checked_add(99))
            .ok_or(FecError::InvalidParameters)? / 100;
        let parity_shards = percentage_parity.max(minimum_parity_shards);
        if data_shards.checked_add(parity_shards).filter(|total| *total <= MAX_SHARDS_PER_BLOCK).is_none() {
            return Err(FecError::TooManyTotalShards { data: data_shards, parity: parity_shards });
        }

        let mut shards = Vec::with_capacity(data_shards + parity_shards);
        for chunk in payload.chunks(shard_size) {
            let mut shard = vec![0_u8; shard_size];
            shard[..chunk.len()].copy_from_slice(chunk);
            shards.push(shard);
        }
        shards.extend((0..parity_shards).map(|_| vec![0_u8; shard_size]));

        if parity_shards > 0 {
            let codec = ReedSolomon::new(data_shards, parity_shards).map_err(|_| FecError::InvalidParameters)?;
            codec.encode(&mut shards).map_err(|_| FecError::EncodeFailed)?;
        }
        Ok(Self { original_payload_len: payload.len(), data_shards, parity_shards, shards })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_parity_and_recovers_missing_data_shards() {
        let payload: Vec<u8> = (0..97).map(|value| value as u8).collect();
        let block = FecBlock::encode(&payload, 16, 25, 1).unwrap();
        assert_eq!(block.data_shards, 7);
        assert_eq!(block.parity_shards, 2);
        assert_eq!(block.shards.len(), 9);
        assert!(block.shards.iter().all(|shard| shard.len() == 16));

        let codec = ReedSolomon::new(block.data_shards, block.parity_shards).unwrap();
        let mut partial: Vec<Option<Vec<u8>>> = block.shards.iter().cloned().map(Some).collect();
        partial[1] = None;
        partial[6] = None;
        codec.reconstruct(&mut partial).unwrap();
        let recovered: Vec<u8> = partial.into_iter().take(block.data_shards)
            .flatten().flat_map(|shard| shard.into_iter()).take(payload.len()).collect();
        assert_eq!(recovered, payload);
    }

    #[test]
    fn zero_parity_is_data_only_and_minimum_parity_is_honored() {
        let payload = [1, 2, 3, 4];
        let none = FecBlock::encode(&payload, 4, 0, 0).unwrap();
        assert_eq!(none.parity_shards, 0);
        assert_eq!(none.shards, vec![payload.to_vec()]);
        let minimum = FecBlock::encode(&payload, 4, 0, 2).unwrap();
        assert_eq!(minimum.parity_shards, 2);
    }

    #[test]
    fn rejects_unbounded_or_invalid_fec_parameters() {
        assert_eq!(FecBlock::encode(&[], 16, 20, 1).unwrap_err(), FecError::EmptyPayload);
        assert_eq!(FecBlock::encode(&[1], 0, 20, 1).unwrap_err(), FecError::ZeroShardSize);
        assert_eq!(FecBlock::encode(&[1], 1, 101, 0).unwrap_err(), FecError::PercentageOutOfRange(101));
        assert_eq!(FecBlock::encode(&vec![0; MAX_SHARDS_PER_BLOCK + 1], 1, 0, 0).unwrap_err(), FecError::TooManyDataShards(MAX_SHARDS_PER_BLOCK + 1));
        assert!(matches!(FecBlock::encode(&[1], 1, 0, MAX_SHARDS_PER_BLOCK), Err(FecError::TooManyTotalShards { .. })));
    }
}
