# Coordinator-owned shared area

Workers do not read or edit this directory. The coordinator may place cross-task ledgers, conflict notes, and the final source index here while merging `results/`.

The worker contract and all shared execution rules live in `00_MASTER.md`, so every Wave A task remains executable with only the master, its assigned task file, the explicitly named repository inputs, and the required primary sources.
