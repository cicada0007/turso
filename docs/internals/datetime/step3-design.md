# Step 3a of #11314: PostgreSQL's clock

Status: implemented with this doc. The rest of step 3 (argument types for dialect functions;
`extract`, `date_part`, `date_trunc`, `to_char` of dates, `make_date`, `pg_typeof`; and
`'now'`, `'today'`, `'tomorrow'`, `'yesterday'` as input) is step 3b, designed with its own PR.

## TLDR

After this step, in tursopg:

```sql
BEGIN;
SELECT now() = transaction_timestamp(), now() = current_timestamp;  -- t | t, in every statement of the block
SELECT now() < statement_timestamp();                               -- t once time has passed since BEGIN
SELECT clock_timestamp() >= statement_timestamp();                  -- t
SELECT current_date = now()::date, localtimestamp = now()::timestamp; -- t | t
COMMIT;
SELECT current_time;                            -- error (PostgreSQL: timetz)
SELECT to_char(timestamp '2024-01-05', 'YYYY'); -- error (NULL before)
SELECT make_date(2024, 1, 5);                   -- error (bytes before)
```

and in tursodb, `now()` and `CREATE TABLE t(ts timestamptz DEFAULT (now())) STRICT`.

Only the clock gives wrong answers on main: after `INSERT INTO a VALUES (now())`,
`ts <= current_timestamp` is false, and one `SELECT now()` over 20,000 rows gives 80
different values (PostgreSQL: true, and 1 value). The rest of step 3 is missing functions and
input words, which give errors, not wrong answers, so it waits for 3b.

## What exists today

- Step 1 (#11492): `date`, `time`, `timestamp`, `timestamptz` are built-in types for both
  dialects, stored as fixed text: `2024-01-05`, `10:30:15.123456`,
  `2024-01-05 10:30:15.123456`, and `timestamptz` in UTC with no suffix.
- The clock: Turso already copies SQLite. SQLite resets `Vdbe.iCurrentTime` (vdbeInt.h:470)
  at the start of each `sqlite3VdbeExec` call, which is one `sqlite3_step` (vdbe.c:956), and
  `sqlite3StmtCurrentTime` reads the OS clock on the first request and keeps it
  (vdbeapi.c:1183). Turso has `ProgramState.current_time`, cleared when a step ends and on
  reset, and read through `StatementClock`. Triggers and FK actions get the parent's value
  (`Statement::set_current_time`).
- tursopg before this step (measured):

  | SQL | tursopg | PostgreSQL 17 |
  |---|---|---|
  | `now()` | `2026-11-10 14:15:56.201`, read on every call | transaction start, µs |
  | `statement_timestamp()`, `transaction_timestamp()` | same as `now()` | statement / transaction start |
  | `current_timestamp`, `localtimestamp` | `2026-11-10 14:15:56` (SQLite's, per step) | = `now()` |
  | `current_date`, `localtime` | SQLite's text | `date`, `time` |
  | `to_char(timestamp '…', 'YYYY')` | NULL | `2024` |
  | `make_date(2024, 1, 5)` | bytes from the time extension | `2024-01-05` |

## 1. The clock

### What PostgreSQL does (measured, `pg_clock.sql`)

Inside `BEGIN; SELECT pg_sleep(0.2); …`:

| Expression | Result |
|---|---|
| `now() = transaction_timestamp()`, `current_timestamp = now()` | t |
| `now() < statement_timestamp()` | t |
| `localtimestamp = now()::timestamp`, `current_date = now()::date`, `localtime = now()::time` | t |
| `statement_timestamp() = statement_timestamp()` | t |
| `clock_timestamp() > statement_timestamp()` | t |
| `length(to_char(now(), 'US'))` | 6 (microseconds) |

So: `now()`, `transaction_timestamp()`, `current_timestamp`, `localtimestamp`, `localtime` and
`current_date` all read the time the transaction started; in autocommit that is the
statement's start. `statement_timestamp()` is fixed for one statement. `clock_timestamp()`
reads the clock on every call.

### Where it goes

SQLite has no transaction time, so there is no SQLite placement for it. It follows what SQLite
does for the step time: the value lives next to the thing it belongs to.

- `ProgramState.statement_times: Option<StatementTimes>` next to `current_time`, holding the
  statement's time and its transaction's time. Both are fixed on the statement's first request
  for the time and kept until reset. Unlike `current_time` they are not cleared when a step
  ends. Copying the transaction time into the statement keeps `now()` the same for the whole
  statement, even when another statement on the connection runs `BEGIN` or `COMMIT` between
  two of its steps.
- `Connection.transaction_time` next to `auto_commit`. `Connection::begin_user_transaction`
  sets it, from the I/O clock, where a user transaction starts: `BEGIN`, a `SAVEPOINT` that
  starts a transaction, and `wal_insert_begin`. That is PostgreSQL's rule: the time of `BEGIN`.
  `Connection::end_transaction` clears it, and every return to autocommit goes through it.
  `Connection::transaction_time` asserts that autocommit has no time.
- The transactions the engine runs for itself, VACUUM and reading the schema back after a
  schema change in autocommit, leave autocommit without a time, so a statement inside one uses
  its own statement time. Reading the schema back inside a user transaction keeps the user's
  time.
- The MVCC restore after an inner transaction of a sequence fails goes back to the outer
  transaction with the time it had, saved with the rest of the outer state
  (`saved_transaction_time`). The inner transaction's error rollback had already ended the
  outer one.
- One method decides what "now" means for the PostgreSQL clock, and `clock_timestamp()` fixes
  the statement time before it reads the clock. They are methods of `ScalarFuncContext`, which
  gets `ProgramState.statement_times` as a `Cell`:

  ```rust
  fn transaction_time(&self) -> WallClockInstant {
      self.statement_times().transaction
  }

  fn clock_time(&self) -> WallClockInstant {
      // statement_timestamp() <= clock_timestamp() whichever the query calls first.
      self.statement_times();
      self.io.current_time_wall_clock()
  }
  ```

  The times are read lazily: a statement that never asks for the time never reads the clock.
  PostgreSQL guarantees `now() <= statement_timestamp() <= clock_timestamp()` in any order of
  evaluation. With the code above: inside a transaction, the transaction time was read at
  `BEGIN`, before any later statement reads its time; in autocommit `now()` is the statement
  time; and every `clock_timestamp()` reads the clock after the statement time is fixed. (All
  three come from the I/O clock; a wall clock that jumps back breaks this in PostgreSQL too.)
- The other choice was to read the statement time eagerly when a statement starts. That is one
  clock read for every statement run, including the ones that never ask for the time, and it
  changes the sequence of clock reads the simulator replays. The cost of the lazy read: a
  statement that first asks for the time after reading many rows gets a later time than
  PostgreSQL's statement start; the order above still holds.
- `BEGIN` reads the clock once per transaction even if nothing asks for the time, because
  PostgreSQL's `now()` is the time of `BEGIN` and can't be read later.
- Triggers and FK actions get the parent's `statement_times` the same way they get
  `current_time` (`Statement::set_statement_times`). The parent's times are fixed first, so a
  trigger that reads the time first can't fix its own copy and leave the parent a different one.
- MVCC: `BEGIN CONCURRENT` is a transaction like any other. The one MVCC-only path is the
  sequence restore above.
- The SQLite functions (`date('now')`, `CURRENT_TIMESTAMP` in tursodb) keep the per-step SQLite
  clock. Only the PostgreSQL functions read `transaction_time`.

Difference from PostgreSQL that this leaves: several statements in one simple-query message
share one transaction in PostgreSQL; tursopg runs each in its own. That is how tursopg runs the
message, not a clock question.

### Functions

Three functions need `ProgramState` (the statement's time), which dialect functions don't get
(`Dialect::exec_scalar_function` receives only the connection). They are core `ScalarFunc`s:

| Function | Type | Value | Name resolved by |
|---|---|---|---|
| `now()` | timestamptz | `transaction_time` | core (both dialects) |
| `transaction_timestamp()` | timestamptz | `transaction_time` (`ScalarFunc::Now`) | `PostgresDialect::resolve_function` |
| `statement_timestamp()` | timestamptz | `statement_time` | `PostgresDialect::resolve_function` |
| `clock_timestamp()` | timestamptz | `clock_time` | `PostgresDialect::resolve_function` |

Only `now` goes in core's name table, so tursodb gets `now()` and nothing else (Q3). Values are
written by step 1's formatter, so `now()` has the same text as a stored `timestamptz`. The
three functions are non-deterministic, so the existing checks already refuse them, and the
casts of them, in index expressions, partial-index WHERE and generated columns.

PRAGMA function_list and tursopg's `pg_proc` list a function only when the dialect resolves
it (`Func::builtin_function_list(dialect)`). The function enums hold functions that only
PostgreSQL SQL can call, such as `statement_timestamp()`, so a list built from the enums alone
would show tursodb functions it refuses. A dialect's own names for engine functions, such as
`transaction_timestamp` for `ScalarFunc::Now`, come from `Dialect::function_names`.

### The time keywords

PostgreSQL defines the keywords by `now()`, and the clock table above shows it
(`localtimestamp = now()::timestamp`, and so on). tursopg stores each keyword as a call of a
function of its own, cast to the keyword's type:

| SQL (tursopg) | stored as | Type |
|---|---|---|
| `CURRENT_TIMESTAMP` | `current_timestamp_keyword()` | timestamptz |
| `CURRENT_DATE` | `CAST(current_date_keyword() AS date)` | date |
| `LOCALTIMESTAMP` | `CAST(localtimestamp_keyword() AS timestamp)` | timestamp (the session zone is UTC) |
| `LOCALTIME` | `CAST(localtime_keyword() AS time)` | time |
| `CURRENT_TIME` | error | (PostgreSQL: timetz) |

- `PostgresDialect::resolve_function` resolves the four functions to `ScalarFunc::Now`.
- Each keyword has its own function so that the catalogs (`pg_attrdef`, `pg_get_expr`) can show
  the keyword that was written, as PostgreSQL does, and not `now()`. The table of keywords and
  functions is `TIME_KEYWORDS` in the tursopg translator.
- PostgreSQL has no functions of these names, so the translator refuses them in user SQL
  (42883), and PRAGMA function_list does not list them.
- A CAST to a custom type runs the value through ENCODE then DECODE, and step 1's `date` and
  `time` input already take a full timestamp (`2024-01-05 10:00:00` → `2024-01-05`).
  `CURRENT_DATE` has type `date` because the CAST's type is.
- `CURRENT_TIME` is an error (Q5): its type is `timetz`, which prints an offset
  (`10:30:15.123456+00`), and returning a `time` would print a different value. A precision
  argument (`localtimestamp(2)`) stays an error.

tursodb: `now()` is a new function name; SQLite has none. `localtimestamp` and `localtime` stay
"no such column" errors (Q2). `CURRENT_DATE`, `CURRENT_TIME` and `CURRENT_TIMESTAMP` in tursodb
stay SQLite's (Q3).

`DEFAULT now()` in tursodb: SQLite's grammar allows only a literal, a signed number, an
identifier or `(expr)` after DEFAULT (parse.y:390-400), so `DEFAULT now()` is a syntax error in
sqlite3 and Turso alike. `DEFAULT (now())` already parses, and SQLite allows a non-constant
function there (`DEFAULT (random())`). This step supports `DEFAULT (now())` and makes no
grammar change (Q1).

## 2. The time extension's functions in tursopg

The time extension (core/time/mod.rs) was registered for both dialects. Most of its names are
`time_*` and `dur_*`, but three are PostgreSQL names with a different meaning: `make_date`,
`make_timestamp` and `to_timestamp` return the extension's 13-byte BLOB. tursopg resolved them
when `PostgresDialect::resolve_function` didn't know the name, so `make_date(2024, 1, 5)`
returned bytes (#11093 is the same for `to_timestamp`).

Refusing those three names in PostgresDialect would be a list that has to follow the extension.
Instead the dialect says which built-in extensions it uses:

- `Dialect::has_builtin_extension(extension: BuiltinExtension) -> bool`, with
  `BuiltinExtension::{Uuid, Series, Time, Percentile, Regexp}`, default `true` (SQLite: all of
  them). PostgresDialect matches every variant, with no `_` arm, and returns `false` for `Time`.
- `register_global_builtin_extensions` asks it before it registers each extension. A database's
  dialect is fixed when it opens, and these registrations are per database, so tursopg never
  has the functions at all. Extensions an application loads are not affected.

tursopg then gives the error it gives for any unknown function for all three. 3b adds
`make_date` with PostgreSQL's meaning; `make_timestamp` and `to_timestamp` stay errors (#11093
open). tursodb keeps all of the time extension.

## 3. Existing bugs this step fixes

- **`to_char(timestamp, text)` returns NULL in tursopg.** `exec_to_char` handles numbers and
  returned `None`, read as NULL, for everything else. Slipup or design smell: design smell, a
  function body that answers NULL for input it does not handle. A value that is not a number
  is now an error (0A000); 3b adds the date forms.
- **`make_date`, `make_timestamp`, `to_timestamp` return bytes in tursopg** (#11093). Slipup or
  design smell: design smell, every function of a SQLite-dialect extension resolves in tursopg.
  The time extension is off in tursopg (section 2).
- **tursopg `now()` reads the clock on every call, with milliseconds.** `now() = now()` can be
  false, two rows of one INSERT can differ, and it keeps milliseconds where PostgreSQL keeps
  microseconds. Slipup or design smell: design smell, the function lives where it can't see the
  statement's clock.
- **tursopg `CURRENT_TIMESTAMP`, `LOCALTIMESTAMP`, `LOCALTIME`, `CURRENT_DATE` are SQLite's:**
  whole seconds, text type, per step. Slipup or design smell: design smell, PostgreSQL keywords
  mapped to SQLite look-alikes with a different meaning.

## 4. Tests

Rust integration tests (tests/integration/), because they need two statements, a wait and
`BEGIN`: the rows of the clock table in section 1, plus: two rows of one `INSERT … SELECT now()`
are equal; `now()` in an `AFTER INSERT` trigger equals the statement's; a new transaction after
`COMMIT`, `ROLLBACK`, outermost `RELEASE` and an error gets a later time; the same under MVCC.

The order of the three clocks, in postgres/conformance/pg-sqltests/ (true on real PostgreSQL),
with `clock_timestamp()` first in the select list so that a lazy `statement_time` read after it
would fail:

```sql
SELECT clock_timestamp() >= statement_timestamp(), statement_timestamp() >= now();  -- t | t
BEGIN;
SELECT clock_timestamp() >= statement_timestamp(), statement_timestamp() >= now();  -- t | t
COMMIT;
```

`now() < statement_timestamp()` inside a transaction needs a wait, so it is in the Rust test.

## 5. Decisions

- **Q1.** `DEFAULT (now())` is enough for tursodb. No grammar change.
- **Q2.** No `localtimestamp`/`localtime` in tursodb. Reading an identifier that names no
  column as a value fails open: a misspelled column becomes the time. tursodb's error today is
  a correct answer.
- **Q3.** tursodb gets only `now()`, which the core function gives for free. Its `CURRENT_*`
  stay SQLite's.
- **Q5.** `CURRENT_TIME` is an error: `timetz` prints an offset, and a `time` would print a
  different value.

(Q4 and Q6 are about `'now'` as input, in 3b.)

## What users lose

Compared with main or step 1:

- tursopg: `make_date`, `make_timestamp`, `to_timestamp` and the time extension's other
  functions (`time_now`, …) are gone; they returned the extension's BLOB. `make_date` comes back
  in 3b with PostgreSQL's meaning.
- tursopg: `to_char` of a value that is not a number is an error, not NULL, until 3b.
- tursopg: `CURRENT_TIME` is an error.

Compared with PostgreSQL:

- A statement's time is read when it first asks for it, which can be later than PostgreSQL's
  statement start; `now() <= statement_timestamp() <= clock_timestamp()` still holds.
- Several statements in one simple-query message each get their own transaction, so their
  `now()` can differ.

## Reviews

A fresh reviewer read the first version and was asked for the smallest change with the same
user-visible result; a second review followed. What changed this step's design:

- **`current_date`, `localtimestamp`, `localtime` as casts**, not core functions of their own.
  A CAST to a custom type is ENCODE then DECODE of the value, and step 1's `date` and `time`
  input take a full timestamp. That also removes the need to tell `CURRENT_TIME` and
  `LOCALTIME` apart in core.
- **Transaction time set at `BEGIN`**, not on first read: it is PostgreSQL's rule. The reviewer
  proposed setting it on every `auto_commit` change from true to false; that would give the
  outer transaction a new time after the MVCC restore, whose inner transaction returned to
  autocommit. Only the three places that start a user transaction set it, and the restore puts
  back the saved time.
- **Transaction time cleared at every return to autocommit** (`Connection::end_transaction`),
  added after the fallback hunt: a time left behind could be read by a later statement whose
  transaction never set one.
- **No `DEFAULT now()` grammar change** in tursodb (Q1).
- **`clock_timestamp()` is a core function.** The first version kept it in tursopg on the
  connection's I/O clock; the second review found that `SELECT clock_timestamp() >=
  statement_timestamp()` was then false, because `statement_timestamp()` fixed the statement
  time after `clock_timestamp()` read the clock. It has to fix the statement time first, which
  needs `ProgramState`.
- **The time extension is off in tursopg**, instead of shadowing `make_date`: the cause is that
  every time-extension function resolved in tursopg, so `make_timestamp` and `to_timestamp`
  stop returning bytes too.
