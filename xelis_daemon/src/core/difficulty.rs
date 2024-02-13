use log::trace;
use xelis_common::difficulty::Difficulty;
use crate::config::{BLOCK_TIME_MILLIS, MINIMUM_DIFFICULTY};

const DIFFICULTY_BOUND_DIVISOR: Difficulty = Difficulty::from_u64(2048);
const CHAIN_TIME_RANGE: u64 = BLOCK_TIME_MILLIS * 2 / 3;

// Difficulty algorithm from Ethereum: Homestead but tweaked for our needs
pub fn calculate_difficulty(tips_count: u64, parent_timestamp: u128, new_timestamp: u128, previous_difficulty: Difficulty) -> Difficulty {
    let mut adjust = previous_difficulty / DIFFICULTY_BOUND_DIVISOR;
    let mut x = (new_timestamp - parent_timestamp) as u64 / CHAIN_TIME_RANGE;
    trace!("x: {x}, tips count: {tips_count}, adjust: {adjust}");
    let neg = x >= tips_count;
    if x == 0 {
        x = x - tips_count;
    } else {
        x = tips_count - x;
    }

    // max(x, 99)
    if x > 99 {
        x = 99;
    }

    let x: Difficulty = x.into();
    // Compute the new diff based on the adjustement
    adjust = adjust * x;
    let new_diff = if neg {
        previous_difficulty - adjust
    } else {
        previous_difficulty + adjust
    };

    trace!("previous diff: {} new diff: {}", previous_difficulty, new_diff);

    if new_diff < MINIMUM_DIFFICULTY {
        return MINIMUM_DIFFICULTY
    }

    new_diff
}