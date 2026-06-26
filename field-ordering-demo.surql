-- ============================================================
-- PRESERVE ORDER — field/key ordering PoC demo
-- Run against a server started with:
--   SURREAL_PATH=memory surreal start --user root --pass root \
--     --bind 127.0.0.1:18066 --allow-experimental field_ordering
-- ============================================================

USE NS test DB test;

-- Seed a few records. Note SET lists fields in non-alphabetical order;
-- internally they are still stored sorted (a,b,c / canonical).
DELETE person;
CREATE person:1 SET name = "Ada",   email = "ada@x.dev",   age = 36, active = true;
CREATE person:2 SET name = "Grace", email = "grace@x.dev", age = 45, active = false;

-- 1) Explicit projection, NO clause  -> alphabetical (today's default)
SELECT name, email, age FROM person;

-- 2) Same projection + PRESERVE ORDER -> fields come back in written order
SELECT name, email, age FROM person PRESERVE ORDER;

-- 3) Reordered projection + PRESERVE ORDER -> honors exactly what you asked for
SELECT age, active, name, email FROM person PRESERVE ORDER;

-- 4) Aliases are honored in written order too
SELECT name AS full_name, age AS years FROM person PRESERVE ORDER;

-- 5) PRESERVE ORDER coexists with the rest of the clause chain
SELECT email, name FROM person WHERE age > 40 ORDER BY name LIMIT 5 PRESERVE ORDER;

-- 6) Equality is order-blind by design (#4053 guardrail):
--    the two objects below are EQUAL even though one is written reversed.
RETURN { a: 1, b: 2, c: 3 } = { c: 3, b: 2, a: 1 };   -- expect: true
