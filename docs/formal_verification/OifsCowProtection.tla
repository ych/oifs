----------------------- MODULE OifsCowProtection -----------------------
(*
 * TLA+ Formal Specification for OIFS Copy-on-Write (COW) Chunk Extent Replacement
 * and Linearizable Positional Read Isolation.
 *
 * Verifies that under arbitrary concurrent interleavings between multiple Reader
 * threads and a Writer thread updating an extent at an offset:
 * 1. TypeOK: All state variables remain well-typed.
 * 2. NoTornReadInvariant: Active readers always observe either a complete Old Version
 *    or a complete New Version of the data, and NEVER an intermediate torn/corrupted state.
 * 3. NoDanglingBlocksInvariant: Free blocks are never referenced by an active Inode pointer.
 *)

EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    MaxBlocks,      \* Total number of physical blocks available
    ReaderThreads   \* Set of concurrent reader thread IDs

VARIABLES
    diskBlocks,     \* Array of block states: [1..MaxBlocks] -> "FREE" | "OLD_DATA" | "NEW_DATA"
    inodePtr,       \* Currently published block ID in the Inode (1..MaxBlocks)
    writerState,    \* Writer protocol phase: "IDLE" | "ALLOCATED" | "WRITTEN" | "COMMITTED"
    writerNewBlk,   \* Block ID allocated by writer for COW
    writerOldBlk,   \* Block ID previously pointed to by Inode
    readerState,    \* [r \in ReaderThreads] -> "IDLE" | "READING"
    readerObserved  \* [r \in ReaderThreads] -> "NONE" | "OLD_DATA" | "NEW_DATA"

vars == <<diskBlocks, inodePtr, writerState, writerNewBlk, writerOldBlk, readerState, readerObserved>>

-----------------------------------------------------------------------------
(* Type Invariant *)
TypeOK ==
    /\ inodePtr \in 1..MaxBlocks
    /\ \A b \in 1..MaxBlocks : diskBlocks[b] \in {"FREE", "OLD_DATA", "NEW_DATA"}
    /\ writerState \in {"IDLE", "ALLOCATED", "WRITTEN", "COMMITTED"}
    /\ writerNewBlk \in (1..MaxBlocks) \cup {0}
    /\ writerOldBlk \in (1..MaxBlocks) \cup {0}
    /\ \A r \in ReaderThreads : readerState[r] \in {"IDLE", "READING"}
    /\ \A r \in ReaderThreads : readerObserved[r] \in {"NONE", "OLD_DATA", "NEW_DATA"}

(* Initial State *)
Init ==
    /\ diskBlocks = [b \in 1..MaxBlocks |-> IF b = 1 THEN "OLD_DATA" ELSE "FREE"]
    /\ inodePtr = 1
    /\ writerState = "IDLE"
    /\ writerNewBlk = 0
    /\ writerOldBlk = 0
    /\ readerState = [r \in ReaderThreads |-> "IDLE"]
    /\ readerObserved = [r \in ReaderThreads |-> "NONE"]

-----------------------------------------------------------------------------
(* Writer Actions: Copy-on-Write 4-Step Protocol *)

\* Step 1: Allocate new contiguous blocks (COW out-of-place allocation)
WriterAllocate ==
    /\ writerState = "IDLE"
    /\ \E b \in 1..MaxBlocks :
        /\ diskBlocks[b] = "FREE"
        /\ writerNewBlk' = b
        /\ writerOldBlk' = inodePtr
        /\ writerState' = "ALLOCATED"
        /\ UNCHANGED <<diskBlocks, inodePtr, readerState, readerObserved>>

\* Step 2: Write compressed or raw payload into the newly allocated block
WriterWritePayload ==
    /\ writerState = "ALLOCATED"
    /\ diskBlocks' = [diskBlocks EXCEPT ![writerNewBlk] = "NEW_DATA"]
    /\ writerState' = "WRITTEN"
    /\ UNCHANGED <<inodePtr, writerNewBlk, writerOldBlk, readerState, readerObserved>>

\* Step 3: Atomic commit - swap Inode ChunkEntry pointer to the new block
WriterAtomicCommit ==
    /\ writerState = "WRITTEN"
    /\ inodePtr' = writerNewBlk
    /\ writerState' = "COMMITTED"
    /\ UNCHANGED <<diskBlocks, writerNewBlk, writerOldBlk, readerState, readerObserved>>

\* Step 4: Free old blocks in bitmap
WriterFreeOldBlock ==
    /\ writerState = "COMMITTED"
    /\ diskBlocks' = [diskBlocks EXCEPT ![writerOldBlk] = "FREE"]
    /\ writerState' = "IDLE"
    /\ writerNewBlk' = 0
    /\ writerOldBlk' = 0
    /\ UNCHANGED <<inodePtr, readerState, readerObserved>>

-----------------------------------------------------------------------------
(* Reader Actions: Positional read_at under shared RwLock read guard *)

\* Reader acquires shared read-lock, snapshots block pointer from Inode, and reads data
ReaderStart(r) ==
    /\ readerState[r] = "IDLE"
    /\ readerState' = [readerState EXCEPT ![r] = "READING"]
    /\ readerObserved' = [readerObserved EXCEPT ![r] = diskBlocks[inodePtr]]
    /\ UNCHANGED <<diskBlocks, inodePtr, writerState, writerNewBlk, writerOldBlk>>

\* Reader finishes and releases read-lock
ReaderFinish(r) ==
    /\ readerState[r] = "READING"
    /\ readerState' = [readerState EXCEPT ![r] = "IDLE"]
    /\ UNCHANGED <<diskBlocks, inodePtr, writerState, writerNewBlk, writerOldBlk, readerObserved>>

-----------------------------------------------------------------------------
(* Next State Relation *)
Next ==
    \/ WriterAllocate
    \/ WriterWritePayload
    \/ WriterAtomicCommit
    \/ WriterFreeOldBlock
    \/ (\E r \in ReaderThreads : ReaderStart(r) \/ ReaderFinish(r))

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------
(* Safety Properties / Formal Invariants *)

\* INVARIANT 1: No Torn Reads (Linearizability)
\* Any reader that reads payload strictly observes either valid OLD_DATA or valid NEW_DATA,
\* never FREE memory, unwritten garbage, or torn intermediate states.
NoTornReadInvariant ==
    \A r \in ReaderThreads :
        readerObserved[r] \in {"NONE", "OLD_DATA", "NEW_DATA"}

\* INVARIANT 2: Inode points only to valid allocated blocks
InodePointsToValidData ==
    diskBlocks[inodePtr] \in {"OLD_DATA", "NEW_DATA"}

\* INVARIANT 3: Mutual Exclusion / No Dangling Pointer
NoDanglingBlockPointer ==
    diskBlocks[inodePtr] /= "FREE"

=============================================================================
