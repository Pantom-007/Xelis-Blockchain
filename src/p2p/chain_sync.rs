use crate::core::blockchain::Blockchain;
use crate::core::difficulty::{check_difficulty, hash_to_big};
use crate::crypto::hash::{Hash, Hashable};
use crate::core::error::BlockchainError;
use crate::core::block::CompleteBlock;
use std::borrow::Cow;
use std::collections::HashMap;


pub struct ChainSync<'a> {
    blocks: HashMap<Hash, Cow<'a, CompleteBlock>>
}

impl<'a> ChainSync<'a> {
    pub fn new() -> Self {
        Self {
            blocks: HashMap::new()
        }
    }

    fn block_exist(&self, hash: &Hash) -> bool {
        self.blocks.contains_key(hash)
    }

    fn get_block(&self, hash: &Hash) -> Result<&CompleteBlock, BlockchainError> {
        match self.blocks.get(hash) {
            Some(v) => Ok(v),
            None => return Err(BlockchainError::BlockNotFound(hash.clone()))
        }
    }

    pub fn insert_block_no_check(&mut self, hash: Hash, block: Cow<'a, CompleteBlock>) {
        self.blocks.insert(hash, block);
    }

    pub fn insert_block(&mut self, block: Cow<'a, CompleteBlock>) -> Result<(), BlockchainError> {
        let block_hash = block.hash();
        if !self.block_exist(&block_hash) { // no need to re verify/insert block
            if !self.block_exist(block.get_previous_hash()) {
                return Err(BlockchainError::BlockNotFound(block.get_previous_hash().clone()))
            }

            if !check_difficulty(&block_hash, block.get_difficulty())? {
                return Err(BlockchainError::InvalidDifficulty)
            }
        }
        self.insert_block_no_check(block_hash, block);
        Ok(())
    }

    pub fn size(&self) -> usize {
        self.blocks.len()
    }
}