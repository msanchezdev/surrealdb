-- ===========================================================================
-- Database Branching PoC — demo script
--
-- 1) Start the server (separate terminal). The --planner-strategy compute-only
--    flag is REQUIRED: the copy-on-write read overlay is wired into the compute
--    executor; the new streaming planner is not yet branch-aware.
--
--    SURREAL_PATH="surrealkv:///tmp/branch-poc" \
--      ./target/debug/surreal start \
--        --allow-experimental database_branching \
--        --planner-strategy compute-only \
--        --user root --pass root --bind 127.0.0.1:8011
--
--    (Versioning is NOT required — the overlay reads the parent's current state.
--     `SURREAL_PATH=memory` works too.)
--
-- 2) Run this script:
--    ./target/debug/surreal sql --endpoint ws://127.0.0.1:8011 \
--        --user root --pass root --ns demo --db main --pretty \
--        < branching-demo.surql
-- ===========================================================================

USE NS demo DB main;

-- 1) Seed the parent database.
CREATE product:laptop SET name = "Laptop", price = 1000;
CREATE product:phone  SET name = "Phone",  price =  500;
CREATE product:tablet SET name = "Tablet", price =  300;

SELECT * FROM product;          --> parent: laptop, phone, tablet

-- 2) Branch `main` into a new database `experiment` (copy-on-write).
DEFINE DATABASE experiment FROM main;

-- 3) The branch transparently sees the parent's rows (nothing was copied).
USE NS demo DB experiment;
SELECT * FROM product;          --> laptop, phone, tablet  (inherited from parent)

-- 4) Diverge the branch with a branch-only product.
CREATE product:keyboard SET name = "Keyboard", price = 80;
SELECT * FROM product;          --> laptop, phone, tablet, keyboard

-- 5) The parent is untouched by the branch's writes (isolation).
USE NS demo DB main;
SELECT * FROM product;          --> laptop, phone, tablet   (NO keyboard)

-- ---------------------------------------------------------------------------
-- PoC scope (works today):
--   * DEFINE DATABASE @branch FROM @source_db [VERSION @datetime]  (branch creation, gated)
--   * Full-table SELECT overlay: branch rows over parent rows, branch wins
--   * Branch-local writes isolated from the parent (separate keyspace)
--
-- Not yet (follow-ups):
--   * New streaming planner (run with --planner-strategy compute-only for now)
--   * Point reads / ranges / index scans overlay; persisted tombstones (branch DELETE)
--   * base_version-pinned reads (branch isolation from FUTURE parent writes)
--   * MERGE DATABASE @branch INTO @target; REMOVE guard; INFO branch metadata
-- ---------------------------------------------------------------------------
