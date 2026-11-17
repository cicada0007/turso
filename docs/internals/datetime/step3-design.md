# Step 3a of #11314: PostgreSQL's clock

Status: step 3a is implemented. Step 3b (`extract`, `date_part`, `date_trunc`, `make_date`) is
implemented as the last section describes. `to_char` of dates, `pg_typeof` and `'now'`, `'today'`,
`'tomorrow'`, `'yesterday'` as input remain.

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

- `StatementScope.statement_times: Option<StatementTimes>` next to `current_time`, holding the
  statement's time and its transaction's time. Both are fixed on the statement's first request
  for the time and kept until reset. Unlike `current_time` they are not cleared when a step
  ends. Copying the transaction time into the statement keeps `now()` the same for the whole
  statement, even when another statement on the connection runs `BEGIN` or `COMMIT` between
  two of its steps.
- The connection's transaction owner, one private value:
  `enum TxnOwner { Autocommit, User { began }, Engine }`. `began` is the transaction time, so
  there is no second field that has to agree with it. `Connection::begin_user_transaction`
  reads the I/O clock where a user transaction starts: `BEGIN`, a `SAVEPOINT` that starts a
  transaction, and `wal_insert_begin`. That is PostgreSQL's rule: the time of `BEGIN`. It
  asserts that the connection is in autocommit, and `wal_insert_begin` refuses to run inside a
  transaction. Every return to autocommit goes through `Connection::end_transaction`.
- The transactions the engine runs for itself, VACUUM and reading the schema back after a
  schema change in autocommit, are `TxnOwner::Engine`, which has no time, so a statement
  inside one uses its own statement time. Reading the schema back inside a user transaction
  keeps the user's time.
- Under MVCC, `nextval` runs in a transaction of its own. When the statement fails before that
  transaction commits, `Program::abort` first ends it and puts the outer transaction back, time
  included (#11679). If its commit had begun, dropping the commit ends it: a rollback on top
  would undo a commit that group commit took over.
- `ScalarFuncContext::statement_times()` reads the statement's times on the first request and
  returns the same ones after that. The context gets `StatementScope.statement_times` as a
  `Cell` in a private field, so no function can write it. tursopg's `clock_timestamp()` fixes
  the statement's times before it reads the clock:

  ```rust
  "clock_timestamp"(FunctionArity::Exact(0), false) => {
      // statement_timestamp() <= clock_timestamp() whichever the query calls first.
      context.statement_times();
      Timestamptz(format_timestamptz(connection.get_pager().io.current_time_wall_clock())?)
  },
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
- Triggers, FK actions and materialized view maintenance get a copy of the parent's
  `StatementScope` (deadline, `current_time`, `statement_times`) before each step, as SQLite's
  run on the parent's VM and read its `iCurrentTime`. `Statement::step_subprogram` takes the
  scope as an argument, so no caller can step a subprogram without one.
  `StatementScope::share` fixes the parent's times first, so a trigger that reads the time
  first can't fix its own copy and leave the parent a different one.
- MVCC: `BEGIN CONCURRENT` is a transaction like any other. The one MVCC-only path is the
  `nextval` transaction above.
- The SQLite functions (`date('now')`, `CURRENT_TIMESTAMP` in tursodb) keep the per-step SQLite
  clock. Only the PostgreSQL functions read `transaction_time`.

Difference from PostgreSQL that this leaves: several statements in one simple-query message
share one transaction in PostgreSQL; tursopg runs each in its own. That is how tursopg runs the
message, not a clock question.

### Functions

The clock functions need the statement's times. Built-in and native functions get the same
`ScalarFuncContext`, which holds them, as every SQLite function gets a `sqlite3_context` and
reads the statement's time through it (`sqlite3StmtCurrentTime`):

| Function | Type | Value | Where |
|---|---|---|---|
| `now()` | timestamptz | `transaction_time` | core `ScalarFunc::Now` (both dialects) |
| `transaction_timestamp()` | timestamptz | `transaction_time` | tursopg native function |
| `statement_timestamp()` | timestamptz | `statement_times().statement` | tursopg native function |
| `clock_timestamp()` | timestamptz | the I/O clock, after `statement_times()` | tursopg native function |

tursodb gets `now()` and nothing else (Q3). Values are written by step 1's formatter
(`format_timestamptz`), so `now()` has the same text as a stored `timestamptz`. The four
functions are non-deterministic, so the existing checks already refuse them, and the casts of
them, in index expressions, partial-index WHERE and generated columns.

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
  outer transaction a new time after `nextval`'s own transaction under MVCC ends. Only the
  three places that start a user transaction set it.
- **The time lives in the transaction owner** (`TxnOwner::User { began }`), after the design
  review: as a field next to `auto_commit` it was a second owner of the same fact, and a time
  left behind could be read by a later statement whose transaction never set one.
- **No `DEFAULT now()` grammar change** in tursodb (Q1).
- **`clock_timestamp()` reads the statement's times.** The first version kept it in tursopg on
  the connection's I/O clock; the second review found that `SELECT clock_timestamp() >=
  statement_timestamp()` was then false, because `statement_timestamp()` fixed the statement
  time after `clock_timestamp()` read the clock. It has to fix the statement time first. A
  later version made the three PostgreSQL clock functions core functions for that; the design
  review moved them back to tursopg, with native functions getting `ScalarFuncContext`.
- **The time extension is off in tursopg**, instead of shadowing `make_date`: the cause is that
  every time-extension function resolved in tursopg, so `make_timestamp` and `to_timestamp`
  stop returning bytes too.

## Step 3b: `extract`, `date_trunc`, `make_date`

```sql
SELECT extract(second FROM timestamp '2024-01-05 10:00:15.5') + 1;  -- 16.500000 (numeric)
SELECT date_trunc('day', date '2024-01-05');                        -- 2024-01-05 00:00:00+00
SELECT make_date(2024, 2, 29);                                      -- 2024-02-29
SELECT date_part('year', date '2024-01-05');                        -- error 0A000
```

### Result types that depend on the arguments

`date_trunc` returns the type of its argument, and each function must refuse arguments
they cannot read correctly before the statement runs. A native function had one fixed result
type, chosen when it was registered, so core gets one new hook:

- `ExternalFunc::choosing_result_type(fn(&[ArgumentType]) -> Result<ReturnType>)`.
  `ArgumentType` is `StringLiteral` (only a string literal itself), `Unknown` (NULL, a
  parameter, or an expression of string literals such as `CASE WHEN c THEN 'day' END`) or
  `Typed(name)`, where the name has domains removed. The function's `ResultType` is then
  `ChosenFromArgumentTypes(choose)`, so every reader of `ResultType` must handle it.
  Translation calls it before it emits the call, so its error is the
  statement's error; `type_of` calls it for the expression's type and treats an error as
  `Unknown`, the type translation then reports the error for. The STRICT CHECK checker refuses
  the call, since its own typer types a string literal as text (#11902).
- The function body still receives the values. With `receiving_argument_types()` it also
  receives each argument's type name, which `extract` and `date_trunc` need to read a stored
  value. A custom type's operator passes only the operands, so it names a core `ScalarFunc`
  (`OperatorSignature::func`), never an external function.

SQLite functions have no result types, but SQLite checks the arguments of a call while it
compiles the statement in the same way: `resolveExprStep` (src/resolve.c:1179-1196) finds the
function by name and argument count, and refuses `unlikely(x, y)` when `y` is not a constant
between 0.0 and 1.0. The hook runs where Turso translates the call, which is later, so a call
is not checked until it is compiled (#11935). The select list of an `EXISTS` subquery, which
the planner drops, is never checked. A view and a column `DEFAULT` are checked when they are
used, not when they are created.

### Values

Arguments are read with step 1's `Temporal::read_stored(text, type)` and results written with
`Temporal::to_text()`, so there is one parser and one formatter for the stored form. A stored
value that does not parse is an error. `extract` returns `numeric` text with PostgreSQL's scale:
6 for `second` and `epoch` (0 for the epoch of a date), 3 for `milliseconds`, else 0.

### Units

As in PostgreSQL's `timestamp_part_common`, `time_part_common`, `extract_date` and
`timestamp_trunc`: the unit is lowercased and compared on its first 10 characters, `extract`
looks in the interval units and then in the date words, and `date_trunc` only in the interval
units. The errors are PostgreSQL's: `0A000 unit "hour" not supported for type date` for a unit
the type has no field for, and `22023 unit "foo" not recognized for type date` for an unknown
one.

### Refused with `0A000`

- `date_part`: its result is `double precision`, which tursopg prints differently (#10695).
  `PostgresDialect::resolve_function` refuses the name, as it would refuse any function it
  doesn't have, but with 0A000 instead of 42883.
- An argument that is a string constant, NULL or parameter: PostgreSQL picks the type from the
  context, which tursopg doesn't do. A `date_trunc` unit that is not a string constant.
- The fields `julian` and `timezone*`, and `date_trunc` of a `time` (PostgreSQL returns an
  `interval`).
- Results outside years 1 to 9999, which the stored form cannot hold: `make_date` of years BC or
  after 9999, and `date_trunc('decade')` of years 1 to 9. Arguments that are out of range in
  PostgreSQL too get PostgreSQL's `22008` error.

tursodb gets none of these functions.
