# XELIS Blockchain

XELIS is a blockchain made in Rust and powered by Tokio, using account model with a unique P2p in TCP sending data in raw bytes format directly.
This project is based on an event-driven system combined with the native async/await.
Its possible to create transactions, sign them, and introduce them in a block. A difficulty adjustment algorithm keeps the average block time to 15 seconds.

## Config

### Daemon

- Default P2P port is `2125`
- Defaut RPC Server port is `8080`

### Wallet

- Default RPC Server port is `8081`

## Roadmap

- Create a functional wallet (WIP)
- Include extra fees when sending coins to a not-yet registered address
- Web Socket for new mining jobs: miner get notified only when the block change.
- Better CLI daemon
- CLI Wallet
- CLI Miner
- Support of Smart Contracts (xelis-vm)
- Privacy (through Homomorphic Encryption)

## BlockDAG

XELIS try to implement & use a blockDAG which the rules are the following:
- A block is considered `Sync Block` when the block height is less than `TOP_HEIGHT - STABLE_HEIGHT_LIMIT` and it's the unique block at a specific height or if it's the heaviest block by cumulative difficulty at its height.
- A block is considered `Side Block` when block height is less than or equal to height of past 8 topographical blocks.
- A block is considered `Orphaned` when the block is not ordered in DAG (no topological height for it).
- A height is not unique anymore.
- Topo height is unique for each block, but can change when the DAG is re-ordered up to `TOP_HEIGHT - STABLE_HEIGHT_LIMIT`.
- You can have up to 3 previous blocks in a block.
- For mining, you have to mine on one of 3 of the most heavier tips.
- Block should not have deviated too much from main chain / heavier tips.
- Maximum 9% of difficulty difference between Tips selected in the same block.
- Side Blocks receive only 30% of block reward.
- Block rewards (with fees) are added to account only when block is in stable height.
- Supply is re-calculated each time the block is re-ordered because its based on topo order.

# Transaction

Transaction types supported:
- Transfer: possibility to send many assets to many addresses in the same TX
- Burn: publicly burn amount of a specific asset and use this TX as proof of burn (coins are completely deleted from circulation)
- Call Contract: call a Smart Contract with specific parameters (WIP) (NOTE: Multi Call Contract in the same TX ?)
- Deploy Contract: deploy a new (valid) Smart Contract on chain (WIP)

At this moment, transactions are public and have the following data.
|   Field   |       Type      |                                   Comment                                  |
|:---------:|:---------------:|:--------------------------------------------------------------------------:|
|   owner   |    PublicKey    |                         Signer of this transaction                         |
|    data   | TransactionType |                 Type with data included of this transaction                |
|    fee    |     Integer     |             Fees to be paid by the owner for including this TX             |
|   nonce   |     Integer     | Matching nonce of balance to be validated and prevent any replay TX attack |
| signature |    Signature    |          Valid signature to prove that the owner validated this TX         |

## Storage

|          Tree         |  Key Type  |    Value Type    |                          Comment                          |
|:---------------------:|:----------:|:----------------:|:---------------------------------------------------------:|
|      transactions     |    Hash    |    Transaction   |        Save the whole transaction based on its hash       |
|         blocks        |    Hash    |   Block Header   |        Save the block header only based on its hash       |
|        rewards        |    Hash    |      Integer     |                   Save the block reward                   |
|         assets        |    Hash    |     No Value     | Used to verify if an assets is well registered and usable |
|         nonces        | Public Key |      Integer     |        Nonce used to prevent replay attacks on TXs        |
|         supply        |    Hash    |      Integer     |   Calculated supply (past + block reward) at each block   |
|       difficulty      |    Hash    |      Integer     |                 Difficulty for each block                 |
|      topo_by_hash     |    Hash    |      Integer     |        Save a block hash at a specific topo height        |
|      hash_by_topo     |   Integer  |       Hash       |        Save a topo height for a specific block hash       |
|    blocks_at_height   |   Integer  |   Array of Hash  |         Save all blocks hash at a specific height         |
|         extra         |    Bytes   | No specific type |   Actually used to save the highest topo height and TIPS  |
| cumulative_difficulty |    Hash    |      Integer     |     Save the cumulative difficulty for each block hash    |

## API

Http Server run using Actix Framework and serve the JSON-RPC API and WebSocket.

JSON-RPC methods available:
- `get_height`
- `get_topoheight`
- `get_stableheight`
- `get_block_template`
- `get_block_at_topoheight`
- `get_blocks_at_height`
- `get_block_by_hash`
- `get_top_block`
- `submit_block`
- `get_nonce`
- `get_balance`
- `get_assets`
- `count_transactions`
- `submit_transaction`
- `get_transaction`
- `p2p_status`
- `get_mempool`
- `get_tips`
- `get_dag_order`

WebSocket allow JSON-RPC call and any app to be notified with `subscribe` method when a specific event happens on the daemon.
Events currently available are:
- `NewBlock`: when a new block is accepted by chain
- `TransactionAddedInMempool`: when a new valid transaction is added in mempool

## XELIS Message

TODO