//! PostgreSQL's `extract`, `date_trunc` and `make_date` for the
//! types `date`, `time`, `timestamp` and `timestamptz`. Values are read and
//! written in the text form the engine stores them in (see
//! `turso_core::Temporal`). The session time zone is always UTC.

use crate::error::sqlstate_error;
use crate::functions::{pg_scalar_function, undefined_function};
use chrono::{Datelike, Days, NaiveDate, NaiveTime, TimeDelta, Timelike};
use turso_core::native_ext::{ArgumentType, FunctionArity, ReturnType, ScalarFuncContext};
use turso_core::{LimboError, OpenOptions, Register, Result, Temporal, TemporalType, Value};
use turso_pg_parser::sqlstate::{
    SQLSTATE_DATETIME_FIELD_OVERFLOW, SQLSTATE_FEATURE_NOT_SUPPORTED,
    SQLSTATE_INVALID_PARAMETER_VALUE, SQLSTATE_NUMERIC_VALUE_OUT_OF_RANGE,
};

pub(crate) fn register_datetime_functions(options: OpenOptions) -> OpenOptions {
    options
        .extension_function(
            pg_scalar_function("extract", FunctionArity::Exact(2), true, exec_extract)
                .receiving_argument_types()
                .choosing_result_type(extract_result_type),
        )
        .extension_function(
            pg_scalar_function("date_trunc", FunctionArity::Exact(2), true, exec_date_trunc)
                .receiving_argument_types()
                .choosing_result_type(date_trunc_result_type),
        )
        .extension_function(
            pg_scalar_function("make_date", FunctionArity::Exact(3), true, exec_make_date)
                .choosing_result_type(make_date_result_type),
        )
}

/// `extract(field FROM source)` returns `numeric`.
fn extract_result_type(args: &[ArgumentType]) -> Result<ReturnType> {
    temporal_source("pg_catalog.extract", args)?;
    Ok(ReturnType::Custom("numeric"))
}

/// `date_trunc(unit, source)` returns the type [`truncated_type`] gives.
fn date_trunc_result_type(args: &[ArgumentType]) -> Result<ReturnType> {
    let source = temporal_source("date_trunc", args)?;
    Ok(ReturnType::Custom(truncated_type(source)?.type_name()))
}

/// The type `date_trunc` truncates a source of type `source` as, and returns.
/// PostgreSQL truncates a `date` as the `timestamptz` it casts it to, and a
/// `time` as an interval.
fn truncated_type(source: TemporalType) -> Result<TemporalType> {
    match source {
        TemporalType::Timestamp => Ok(TemporalType::Timestamp),
        TemporalType::Date | TemporalType::Timestamptz => Ok(TemporalType::Timestamptz),
        TemporalType::Time => Err(sqlstate_error(
            SQLSTATE_FEATURE_NOT_SUPPORTED,
            "date_trunc of a time is not supported, since PostgreSQL returns an interval",
        )),
    }
}

/// Checks the arguments of `function(unit, source)` and returns the type of
/// `source`. The unit must be a string constant, as in `'day'`, and the source
/// a date or time value whose type the query states.
fn temporal_source(function: &str, args: &[ArgumentType]) -> Result<TemporalType> {
    let [unit, source] = args else {
        unreachable!("{function} is declared with two arguments");
    };
    let source = match source {
        ArgumentType::Typed(name) => match TemporalType::from_name(name) {
            Some(source) => source,
            None => return Err(undefined_function(function, &pg_type_names(args))),
        },
        ArgumentType::StringLiteral | ArgumentType::Unknown => {
            return Err(sqlstate_error(
                SQLSTATE_FEATURE_NOT_SUPPORTED,
                format!(
                    "{function} needs a value of a stated type, such as CAST('2024-01-05' AS date), not a string constant, NULL or parameter"
                ),
            ))
        }
    };
    match unit {
        ArgumentType::StringLiteral => Ok(source),
        ArgumentType::Typed(_) | ArgumentType::Unknown => Err(sqlstate_error(
            SQLSTATE_FEATURE_NOT_SUPPORTED,
            format!("{function} needs a string constant as its unit, such as 'day'"),
        )),
    }
}

/// `make_date(year, month, day)` takes three integers and returns a `date`.
fn make_date_result_type(args: &[ArgumentType]) -> Result<ReturnType> {
    let is_integer =
        |arg: &ArgumentType| matches!(arg, ArgumentType::Typed(name) if name == "integer");
    if args
        .iter()
        .any(|arg| matches!(arg, ArgumentType::Typed(_)) && !is_integer(arg))
    {
        return Err(undefined_function("make_date", &pg_type_names(args)));
    }
    if !args.iter().all(is_integer) {
        return Err(sqlstate_error(
            SQLSTATE_FEATURE_NOT_SUPPORTED,
            "make_date needs integer values of a stated type, not string constants, NULL or parameters",
        ));
    }
    Ok(ReturnType::Custom("date"))
}

/// The names PostgreSQL would show for `args` in a "function does not exist"
/// error. It calls the type of a string constant `unknown`.
fn pg_type_names(args: &[ArgumentType]) -> Vec<&str> {
    args.iter()
        .map(|arg| match arg {
            ArgumentType::Typed(name) => name.as_str(),
            ArgumentType::StringLiteral | ArgumentType::Unknown => "unknown",
        })
        .collect()
}

/// The type of a call's source, from the type names that follow the
/// arguments. The result type chooser accepts only date and time types.
fn source_of_call(type_name: &Register) -> TemporalType {
    let Value::Text(name) = type_name.get_value() else {
        unreachable!("argument type names are text");
    };
    let Some(source) = TemporalType::from_name(name.as_str()) else {
        unreachable!("the result type chooser refuses a source of type {name}");
    };
    source
}

/// The name PostgreSQL's `format_type_be` gives the type in error messages.
fn pg_message_name(ty: TemporalType) -> &'static str {
    match ty {
        TemporalType::Date => "date",
        TemporalType::Time => "time without time zone",
        TemporalType::Timestamp => "timestamp without time zone",
        TemporalType::Timestamptz => "timestamp with time zone",
    }
}

/// `extract(field FROM source)`: the field as a `numeric`. Seconds have six
/// digits after the decimal point, milliseconds three, and the epoch of a
/// value with a time six, as in PostgreSQL.
fn exec_extract(_: &ScalarFuncContext<'_>, args: &[Register]) -> Result<Value> {
    let (args, types) = args.split_at(args.len() / 2);
    let source = source_of_call(&types[1]);
    let value = args[1].get_value();
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    let unit = UnitText::new(constant_unit(&args[0]));
    let field = extract_field(&unit, source)?;
    let (number, scale) = match (field, read_value(value, source)?) {
        (Field::Time(unit), Temporal::Time(micros)) => time_field(unit, micros),
        (Field::Time(unit), Temporal::Timestamp(at) | Temporal::Timestamptz(at)) => {
            time_field(unit, micros_since_midnight(at.time()))
        }
        (Field::Date(unit), Temporal::Date(date)) => (date_field(unit, date), 0),
        (Field::Date(unit), Temporal::Timestamp(at) | Temporal::Timestamptz(at)) => {
            (date_field(unit, at.date()), 0)
        }
        (Field::Epoch, Temporal::Date(date)) => {
            (date.signed_duration_since(unix_epoch()).num_seconds(), 0)
        }
        (Field::Epoch, Temporal::Time(micros)) => (micros, 6),
        (Field::Epoch, Temporal::Timestamp(at) | Temporal::Timestamptz(at)) => {
            let micros = at.signed_duration_since(unix_epoch().and_time(NaiveTime::MIN));
            let Some(micros) = micros.num_microseconds() else {
                unreachable!("years 1 to 9999 are within 2^63 microseconds of 1970");
            };
            (micros, 6)
        }
        (Field::Time(_), Temporal::Date(_)) | (Field::Date(_), Temporal::Time(_)) => {
            unreachable!("extract_field refuses time fields of a date and date fields of a time")
        }
    };
    Ok(Value::build_text(numeric_text(number, scale)))
}

/// A field `extract` reads.
#[derive(Debug, Clone, Copy)]
enum Field {
    Time(TimeUnit),
    Date(DateUnit),
    /// Seconds since 1970-01-01 00:00 UTC, or since midnight for a time.
    Epoch,
}

/// Finds the field `unit` names for a source of type `source`, as
/// PostgreSQL's `timestamp_part_common`, `time_part_common` and
/// `extract_date` do in src/backend/utils/adt/timestamp.c and date.c: first in
/// the table of interval units, then in the table of date words.
fn extract_field(unit: &UnitText, source: TemporalType) -> Result<Field> {
    let word = match interval_unit(unit.key()) {
        Some(found) => Some(Word::Unit(found)),
        None => date_word(unit.key()),
    };
    match word {
        Some(Word::Unit(Unit::Time(found))) if source != TemporalType::Date => {
            Ok(Field::Time(found))
        }
        Some(Word::Unit(Unit::Date(found))) if source != TemporalType::Time => {
            Ok(Field::Date(found))
        }
        Some(Word::Unit(_)) => Err(unit.not_supported(source)),
        Some(Word::Epoch) => Ok(Field::Epoch),
        // PostgreSQL's time_part_common has no case for the other special
        // values, so it reports them as not recognized.
        Some(Word::Reserved) if source != TemporalType::Time => Err(unit.not_supported(source)),
        Some(Word::Reserved) | None => Err(unit.not_recognized(source)),
    }
}

fn time_field(unit: TimeUnit, micros_since_midnight: i64) -> (i64, u32) {
    let micros_of_minute = micros_since_midnight % 60_000_000;
    match unit {
        TimeUnit::Microseconds => (micros_of_minute, 0),
        TimeUnit::Milliseconds => (micros_of_minute, 3),
        TimeUnit::Second => (micros_of_minute, 6),
        TimeUnit::Minute => (micros_since_midnight / 60_000_000 % 60, 0),
        // `24:00:00` is hour 24.
        TimeUnit::Hour => (micros_since_midnight / 3_600_000_000, 0),
    }
}

/// A field of a date. Stored dates have years 1 to 9999, so the formulas
/// leave out PostgreSQL's cases for years BC.
fn date_field(unit: DateUnit, date: NaiveDate) -> i64 {
    let year = i64::from(date.year());
    let month = i64::from(date.month());
    match unit {
        DateUnit::Day => i64::from(date.day()),
        DateUnit::Week => i64::from(date.iso_week().week()),
        DateUnit::Month => month,
        DateUnit::Quarter => (month - 1) / 3 + 1,
        DateUnit::Year => year,
        DateUnit::Decade => year / 10,
        DateUnit::Century => (year + 99) / 100,
        DateUnit::Millennium => (year + 999) / 1000,
        DateUnit::DayOfWeek => i64::from(date.weekday().num_days_from_sunday()),
        DateUnit::IsoDayOfWeek => i64::from(date.weekday().number_from_monday()),
        DateUnit::DayOfYear => i64::from(date.ordinal()),
        DateUnit::IsoYear => i64::from(date.iso_week().year()),
    }
}

/// `date_trunc(unit, source)`: the source with every field smaller than the
/// unit set to its lowest value.
fn exec_date_trunc(_: &ScalarFuncContext<'_>, args: &[Register]) -> Result<Value> {
    let (args, types) = args.split_at(args.len() / 2);
    let source = source_of_call(&types[1]);
    let value = args[1].get_value();
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    // Errors name the type the source is truncated as.
    let Ok(result_type) = truncated_type(source) else {
        unreachable!(
            "date_trunc_result_type refuses a {} source",
            source.type_name()
        );
    };
    let unit = UnitText::new(constant_unit(&args[0]));
    // Unlike extract, date_trunc looks only in the table of interval units.
    let unit = match interval_unit(unit.key()) {
        Some(Unit::Time(found)) => Truncate::Time(found),
        Some(Unit::Date(found)) => Truncate::Date(found),
        Some(Unit::Unsupported) => return Err(unit.not_supported(result_type)),
        None => return Err(unit.not_recognized(result_type)),
    };
    let at = match read_value(value, source)? {
        Temporal::Date(date) => date.and_time(NaiveTime::MIN),
        Temporal::Timestamp(at) | Temporal::Timestamptz(at) => at,
        Temporal::Time(_) => unreachable!("truncated_type refuses a time"),
    };
    let truncated = match unit {
        Truncate::Time(unit) => {
            let micros = micros_since_midnight(at.time());
            let step = match unit {
                TimeUnit::Microseconds => 1,
                TimeUnit::Milliseconds => 1_000,
                TimeUnit::Second => 1_000_000,
                TimeUnit::Minute => 60_000_000,
                TimeUnit::Hour => 3_600_000_000,
            };
            at.date().and_time(NaiveTime::MIN) + TimeDelta::microseconds(micros - micros % step)
        }
        Truncate::Date(unit) => truncate_date(unit, at.date())?.and_time(NaiveTime::MIN),
    };
    let truncated = match result_type {
        TemporalType::Timestamp => Temporal::Timestamp(truncated),
        TemporalType::Timestamptz => Temporal::Timestamptz(truncated),
        TemporalType::Date | TemporalType::Time => {
            unreachable!("truncated_type returns only timestamp types")
        }
    };
    Ok(Value::build_text(truncated.to_text()))
}

/// A unit `date_trunc` truncates to.
#[derive(Debug, Clone, Copy)]
enum Truncate {
    Time(TimeUnit),
    Date(DateUnit),
}

/// The first day of the period `unit` that holds `date`, as PostgreSQL's
/// `timestamp_trunc` computes it. Weeks start on Monday, and centuries and
/// millennia start in years ending in 01 and 001.
fn truncate_date(unit: DateUnit, date: NaiveDate) -> Result<NaiveDate> {
    let year = date.year();
    let first_day = |year: i32, month: u32| {
        let Some(day) = NaiveDate::from_ymd_opt(year, month, 1) else {
            unreachable!("{year}-{month}-01 is a date");
        };
        day
    };
    Ok(match unit {
        DateUnit::Day => date,
        DateUnit::Week => date - Days::new(u64::from(date.weekday().num_days_from_monday())),
        DateUnit::Month => first_day(year, date.month()),
        DateUnit::Quarter => first_day(year, (date.month() - 1) / 3 * 3 + 1),
        DateUnit::Year => first_day(year, 1),
        // The decade of years 1 to 9 starts in year 0, which is 1 BC.
        DateUnit::Decade if year < 10 => {
            return Err(sqlstate_error(
                SQLSTATE_FEATURE_NOT_SUPPORTED,
                "date_trunc('decade') of a year before 10 gives a year BC, which tursopg does not support",
            ))
        }
        DateUnit::Decade => first_day(year / 10 * 10, 1),
        DateUnit::Century => first_day((year + 99) / 100 * 100 - 99, 1),
        DateUnit::Millennium => first_day((year + 999) / 1000 * 1000 - 999, 1),
        DateUnit::DayOfWeek | DateUnit::IsoDayOfWeek | DateUnit::DayOfYear | DateUnit::IsoYear => {
            unreachable!("{unit:?} is a date word, and date_trunc reads only interval units")
        }
    })
}

/// `make_date(year, month, day)`. A negative year is a year BC, as in
/// PostgreSQL's `make_date` in src/backend/utils/adt/date.c, but only years 1
/// to 9999 can be stored.
fn exec_make_date(_: &ScalarFuncContext<'_>, args: &[Register]) -> Result<Value> {
    // tursopg doesn't check the range of integer arithmetic (#11447), so a value
    // of type integer can be past 32 bits, or past 64 bits and stored as a float.
    let out_of_range =
        || sqlstate_error(SQLSTATE_NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range");
    let mut fields = [0; 3];
    for (field, arg) in fields.iter_mut().zip(args) {
        *field = match arg.get_value() {
            Value::Null => return Ok(Value::Null),
            Value::Numeric(turso_core::Numeric::Integer(value)) => {
                i32::try_from(*value).map_err(|_| out_of_range())?
            }
            Value::Numeric(turso_core::Numeric::Float(_)) => return Err(out_of_range()),
            other => {
                return Err(LimboError::InternalError(format!(
                    "make_date got {other:?} for a value of type integer"
                )))
            }
        };
    }
    let [input_year, month, day] = fields;
    let field_out_of_range = |year: i32| {
        sqlstate_error(
            SQLSTATE_DATETIME_FIELD_OVERFLOW,
            format!("date field value out of range: {year}-{month:02}-{day:02}"),
        )
    };
    // PostgreSQL numbers 1 BC as year 0 and 2 BC as year -1, and its errors
    // show the year in that numbering. There is no year 0 in the input.
    let year = match input_year {
        0 | i32::MIN => return Err(field_out_of_range(input_year)),
        bc if bc < 0 => bc + 1,
        ad => ad,
    };
    let (Ok(month), Ok(day)) = (u32::try_from(month), u32::try_from(day)) else {
        return Err(field_out_of_range(year));
    };
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return Err(field_out_of_range(year));
    }
    // PostgreSQL's dates run from 4714-11-24 BC to 5874897-12-31.
    if (year, month, day) < (-4713, 11, 24) || (year, month, day) > (5_874_897, 12, 31) {
        return Err(sqlstate_error(
            SQLSTATE_DATETIME_FIELD_OVERFLOW,
            format!("date out of range: {year}-{month:02}-{day:02}"),
        ));
    }
    if !(1..=9999).contains(&year) {
        return Err(sqlstate_error(
            SQLSTATE_FEATURE_NOT_SUPPORTED,
            format!("make_date of year {input_year} is not supported; tursopg stores only years 1 to 9999"),
        ));
    }
    let Some(date) = NaiveDate::from_ymd_opt(year, month, day) else {
        unreachable!("{year}-{month}-{day} was checked to be a date");
    };
    Ok(Value::build_text(Temporal::Date(date).to_text()))
}

/// The number of days in `month` of `year` in the proleptic Gregorian
/// calendar, where year 0 is a leap year.
fn days_in_month(year: i32, month: u32) -> u32 {
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => unreachable!("month {month} was checked to be 1 to 12"),
    }
}

/// The unit argument. The result type chooser accepts only a string constant.
fn constant_unit(arg: &Register) -> &str {
    let Value::Text(unit) = arg.get_value() else {
        unreachable!("the result type chooser accepts only a string constant as the unit");
    };
    unit.as_str()
}

/// Reads a value from its stored text. A stored value in any other form is an
/// error.
fn read_value(value: &Value, source: TemporalType) -> Result<Temporal> {
    match value {
        Value::Text(text) => Temporal::read_stored(text.as_str(), source),
        other => Err(LimboError::InternalError(format!(
            "a {source:?} value is stored as text, not as {other:?}"
        ))),
    }
}

fn micros_since_midnight(time: NaiveTime) -> i64 {
    i64::from(time.num_seconds_from_midnight()) * 1_000_000 + i64::from(time.nanosecond() / 1000)
}

fn unix_epoch() -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).expect("1970-01-01 is a date")
}

/// `number / 10^scale` with exactly `scale` digits after the decimal point,
/// as PostgreSQL prints a numeric of that scale: `-0.500000`.
fn numeric_text(number: i64, scale: u32) -> String {
    if scale == 0 {
        return number.to_string();
    }
    let divisor = 10u64.pow(scale);
    let sign = if number < 0 { "-" } else { "" };
    let magnitude = number.unsigned_abs();
    format!(
        "{sign}{}.{:0width$}",
        magnitude / divisor,
        magnitude % divisor,
        width = scale as usize
    )
}

/// The unit as PostgreSQL reads it: lowercased, and cut to 63 bytes as
/// `downcase_truncate_identifier` does. Errors show this text.
struct UnitText(String);

impl UnitText {
    fn new(unit: &str) -> Self {
        let mut lower = unit.to_ascii_lowercase();
        let mut end = lower.len().min(63);
        while !lower.is_char_boundary(end) {
            end -= 1;
        }
        lower.truncate(end);
        Self(lower)
    }

    /// The bytes PostgreSQL compares with its tables: the first 10
    /// (`TOKMAXLEN`), so `microseconds` matches `microsecon`.
    fn key(&self) -> &[u8] {
        let bytes = self.0.as_bytes();
        &bytes[..bytes.len().min(10)]
    }

    fn not_supported(&self, source: TemporalType) -> LimboError {
        sqlstate_error(
            SQLSTATE_FEATURE_NOT_SUPPORTED,
            format!(
                "unit \"{}\" not supported for type {}",
                self.0,
                pg_message_name(source)
            ),
        )
    }

    fn not_recognized(&self, source: TemporalType) -> LimboError {
        sqlstate_error(
            SQLSTATE_INVALID_PARAMETER_VALUE,
            format!(
                "unit \"{}\" not recognized for type {}",
                self.0,
                pg_message_name(source)
            ),
        )
    }
}

/// What a unit names in one of PostgreSQL's tables.
#[derive(Debug, Clone, Copy)]
enum Word {
    Unit(Unit),
    Epoch,
    /// Another special value, such as `now` or `infinity`.
    Reserved,
}

#[derive(Debug, Clone, Copy)]
enum Unit {
    Time(TimeUnit),
    Date(DateUnit),
    /// A unit PostgreSQL has that tursopg refuses: `julian` and the time zone
    /// fields.
    Unsupported,
}

#[derive(Debug, Clone, Copy)]
enum TimeUnit {
    Microseconds,
    Milliseconds,
    Second,
    Minute,
    Hour,
}

#[derive(Debug, Clone, Copy)]
enum DateUnit {
    Day,
    Week,
    Month,
    Quarter,
    Year,
    Decade,
    Century,
    Millennium,
    DayOfWeek,
    IsoDayOfWeek,
    DayOfYear,
    IsoYear,
}

/// The units in PostgreSQL's table of interval units, `deltatktbl` in
/// src/backend/utils/adt/datetime.c. Its other entries, `@` and `ago`, are
/// not units, and both functions treat them as unknown words.
fn interval_unit(key: &[u8]) -> Option<Unit> {
    use DateUnit::*;
    use TimeUnit::*;
    Some(match key {
        b"c" | b"cent" | b"centuries" | b"century" => Unit::Date(Century),
        b"d" | b"day" | b"days" => Unit::Date(Day),
        b"dec" | b"decade" | b"decades" | b"decs" => Unit::Date(Decade),
        b"h" | b"hour" | b"hours" | b"hr" | b"hrs" => Unit::Time(Hour),
        b"m" | b"min" | b"mins" | b"minute" | b"minutes" => Unit::Time(Minute),
        b"microsecon" | b"us" | b"usec" | b"usecond" | b"useconds" | b"usecs" => {
            Unit::Time(Microseconds)
        }
        b"mil" | b"millennia" | b"millennium" | b"mils" => Unit::Date(Millennium),
        b"millisecon" | b"ms" | b"msec" | b"msecond" | b"mseconds" | b"msecs" => {
            Unit::Time(Milliseconds)
        }
        b"mon" | b"mons" | b"month" | b"months" => Unit::Date(Month),
        b"qtr" | b"quarter" => Unit::Date(Quarter),
        b"s" | b"sec" | b"second" | b"seconds" | b"secs" => Unit::Time(Second),
        b"timezone" | b"timezone_h" | b"timezone_m" => Unit::Unsupported,
        b"w" | b"week" | b"weeks" => Unit::Date(Week),
        b"y" | b"year" | b"years" | b"yr" | b"yrs" => Unit::Date(Year),
        _ => return None,
    })
}

/// The units and special values in PostgreSQL's table of date words,
/// `datetktbl` in src/backend/utils/adt/datetime.c, that the table of
/// interval units doesn't already have. Its other entries, such as month
/// names, are neither, and `extract` treats them as unknown words.
fn date_word(key: &[u8]) -> Option<Word> {
    Some(match key {
        b"dow" => Word::Unit(Unit::Date(DateUnit::DayOfWeek)),
        b"doy" => Word::Unit(Unit::Date(DateUnit::DayOfYear)),
        b"isodow" => Word::Unit(Unit::Date(DateUnit::IsoDayOfWeek)),
        b"isoyear" => Word::Unit(Unit::Date(DateUnit::IsoYear)),
        b"mm" => Word::Unit(Unit::Time(TimeUnit::Minute)),
        b"j" | b"jd" | b"julian" => Word::Unit(Unit::Unsupported),
        b"epoch" => Word::Epoch,
        b"infinity" | b"+infinity" | b"-infinity" | b"allballs" | b"now" | b"today"
        | b"tomorrow" | b"yesterday" => Word::Reserved,
        _ => return None,
    })
}
