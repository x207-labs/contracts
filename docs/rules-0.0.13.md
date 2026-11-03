# Interface 0.0.13: the log budget of a receipt and the limit of 300 edges (CT-099)

Status: built in `contracts`, tested in the unit tests and in near-sandbox (`ct099_commit_log_size`, `ct099_hash_vectors`). Release: not yet.

## 1. The rule
NEAR refuses a receipt whose logs together exceed **16,384 bytes** (`HostError::TotalLogLengthExceeded`; the limit is on the total, not on one message). The contract keeps every call under **90 percent of it (14,745 bytes)** at the largest size that it accepts, with account names of 64 characters. The consequence that matters most: **every proposal that `submit_clearing` accepts can be committed by `commit_epoch`.**

## 2. What each call logs (0.0.13)
| Call | Logs | Size at the largest input (names of 64 characters) |
| --- | --- | --- |
| `submit_clearing` | `clearing_submitted` (a summary: `edge_count`, no edges) | fixed, about 400 bytes |
| `commit_epoch` | `clearing_applied` (summary: `edge_count`, `edges_hash`) and `epoch_committed` (all positions, at most 64, with `position_count` and `positions_hash`) | 5.3 KB at 300 edges and 30 members; 10.6 KB at 256 edges and 64 members |
| `finalize_epoch` | `member_defaulted` (cut when the room runs out, with one plain line that says how many), `limits_updated` (chunks of 50), `epoch_finalized` | within 90 percent by construction (`LOG_LIMIT` in `settlement.rs`) |
| `commit_bundle` | `bundle_committed` (at most 64 legs), and the bond events of a deal | 11.1 KB at 64 legs |

The edges of a proposal are **not** in a log. A reader takes them from the view `get_proposal(epoch, from_index, limit)` before `commit_epoch` and from `list_obligations` (`cleared`) after it, and checks them with `edges_hash`. The positions are in `epoch_committed` and are checked with `positions_hash`. The definition of both hashes and a golden vector are in `tests/vectors/clearing_hashes.json` (`scripts/clearing_hashes.py` writes and checks it).

## 3. The limit of 300 edges per proposal
`MAX_EDGES_PER_TX` = 300 stays. It is not a log limit any more: `commit_epoch` at 300 edges uses about 104 Tgas and 5.3 KB of log. What it costs a dense circle is in [`solver/docs/benchmark.md`](../../solver/docs/benchmark.md) (SV-019): the optimum of a random graph of 200 members has thousands of edges (11,400 at 30 percent density), the solver then cuts it to 300 edges by dropping whole cycles, and the share of the debt that one epoch clears falls from 92.4 percent to 3.8 percent. Where the optimum has at most 300 edges nothing is lost. The ways to clear more are a higher limit (gas of `submit_clearing` and `commit_epoch` grow with the edges, about 0.3 Tgas per edge in `commit_epoch`, so 300 Tgas would be reached at about 900 edges, and storage grows too: about 50 bytes per edge) or a second proposal per epoch; neither is in 0.0.13.

## 4. What readers must change
- The SDK event parsers: `clearing_applied` and `epoch_committed` have new fields and no `chunk` and `chunks`.
- The indexer: read the edges from `get_proposal` at the time of the commit (or from `list_obligations` later, while the epoch is not pruned) and positions from `epoch_committed`; verify with the hashes.
- Note for the indexer: after `prune_epoch` the edges of an epoch are no longer readable from the contract, so an indexer that was not following in the epoch must have stored them before.
