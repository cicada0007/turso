use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use turso_core::security::roles::RoleId;
use turso_core::{Connection, ConnectionHooks, SavepointMemo};

use crate::session::{session_state, CustomName};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingScope {
    Session,
    Transaction,
}

pub(crate) struct PgSessionState {
    settings: Mutex<ConnectionSettings>,
}

impl PgSessionState {
    /// The state of a session whose client sent `startup_parameters`. A
    /// parameter's startup value is also the value RESET returns it to, as in
    /// PostgreSQL.
    pub(crate) fn new(startup_parameters: &BTreeMap<String, String>) -> turso_core::Result<Self> {
        let mut settings = ConnectionSettings::default();
        for parameter in PARAMETERS {
            match parameter.kind {
                Kind::Stored { default, .. } => settings.define(parameter, default.to_owned()),
                Kind::SearchPath | Kind::Role => {}
            }
        }
        // PostgreSQL applies the last of two spellings of one name, such as
        // TimeZone and timezone. The map is sorted, so the order the client
        // sent them in is lost, and a name given twice is refused.
        let mut given = HashSet::new();
        for (name, value) in startup_parameters {
            if !given.insert(SettingKey::new(name)) {
                return Err(feature_not_supported(format!(
                    "giving the startup parameter \"{name}\" more than once is not supported"
                )));
            }
            define_startup_value(&mut settings, name, value)?;
        }
        Ok(Self {
            settings: Mutex::new(settings),
        })
    }

    pub(crate) fn mutate(&self, conn: &Connection, mutate: impl FnOnce(&mut ConnectionSettings)) {
        let mut settings = self.settings.lock().unwrap();
        settings.save_memo_before_change(conn.has_active_root_statement(), !conn.get_auto_commit());
        mutate(&mut settings);
    }

    pub(crate) fn setting(&self, name: &str) -> Option<String> {
        self.settings.lock().unwrap().get(name).map(str::to_owned)
    }

    pub(crate) fn has_setting(&self, name: &str) -> bool {
        self.settings.lock().unwrap().contains(name)
    }

    /// The name of the role set with SET ROLE, or `None` if the session's
    /// login role is current.
    pub(crate) fn role_name(&self) -> Option<String> {
        let settings = self.settings.lock().unwrap();
        settings
            .current
            .role
            .current()
            .as_ref()
            .map(|role| role.name.clone())
    }

    pub(crate) fn name_as_defined(&self, name: &str) -> Option<String> {
        self.settings
            .lock()
            .unwrap()
            .names_as_defined
            .get(&SettingKey::new(name))
            .cloned()
    }

    fn update_settings(
        &self,
        conn: &Connection,
        update: impl FnOnce(&mut ConnectionSettings) -> Option<CoreValues>,
    ) {
        let changed = update(&mut self.settings.lock().unwrap());
        if let Some(values) = changed {
            values.apply(conn);
        }
    }

    fn has_changes_to_end(&self) -> bool {
        self.settings.lock().unwrap().has_changes_to_end()
    }
}

impl ConnectionHooks for PgSessionState {
    fn root_statement_ended(&self, conn: &Connection, failed: bool) {
        if !self.has_changes_to_end() {
            return;
        }
        let transaction_ended = conn.get_auto_commit();
        self.update_settings(conn, |settings| {
            settings.end_statement(failed, transaction_ended)
        });
    }

    fn transaction_rolled_back(&self, conn: &Connection) {
        if !self.has_changes_to_end() {
            return;
        }
        self.update_settings(conn, ConnectionSettings::rollback_transaction);
    }

    fn savepoint_created(&self, _conn: &Connection) -> SavepointMemo {
        Arc::new(self.settings.lock().unwrap().current_values().clone())
    }

    fn rolled_back_to_savepoint(&self, conn: &Connection, memo: &SavepointMemo) {
        let saved = memo
            .downcast_ref::<SettingValues>()
            .expect("savepoint_created stores SettingValues")
            .clone();
        self.update_settings(conn, |settings| settings.rollback_to_savepoint(saved));
    }
}

#[derive(Default)]
pub(crate) struct ConnectionSettings {
    current: SettingValues,
    memo_before_transaction: Option<SettingValues>,
    memo_before_statement: Option<SettingValues>,
    /// Each parameter's name as written when it was first set. SHOW names
    /// its column this way. A rollback keeps the parameter defined, so it
    /// keeps this name too.
    names_as_defined: HashMap<SettingKey, String>,
    /// The value RESET gives each parameter that has one: its startup value
    /// or its default.
    reset_values: HashMap<SettingKey, String>,
}

/// A parameter name in ASCII lowercase, since the case of a parameter name
/// does not matter in PostgreSQL.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SettingKey(String);

impl SettingKey {
    fn new(name: &str) -> Self {
        Self(name.to_ascii_lowercase())
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SettingValues {
    search_path: Setting<Option<Vec<String>>>,
    /// The role set with SET ROLE, or `None` for the session's login role.
    role: Setting<Option<SetRole>>,
    /// Stored and custom parameters. Also contains the `search_path` as the user entered it.
    named: HashMap<SettingKey, Setting<Option<String>>>,
    /// whether this struct contains any LOCAL values
    has_transaction_values: bool,
}

/// A role that SET ROLE made current.
#[derive(Clone, Debug)]
pub(crate) struct SetRole {
    pub(crate) id: RoleId,
    /// The name as it was set, which SHOW role returns.
    pub(crate) name: String,
}

/// The values of the settings that core keeps a copy of, so core must be
/// told when one of them changes.
#[derive(Clone, PartialEq)]
struct CoreValues {
    search_path: Option<Vec<String>>,
    role: Option<RoleId>,
}

impl CoreValues {
    fn apply(self, conn: &Connection) {
        conn.set_search_path(self.search_path);
        conn.set_current_role(self.role.unwrap_or_else(|| conn.session_role()));
    }
}

#[derive(Clone, Debug, Default)]
struct Setting<T> {
    session_value: T,
    transaction_value: Option<T>,
}

impl ConnectionSettings {
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.current
            .named
            .get(&SettingKey::new(name))?
            .current()
            .as_deref()
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.names_as_defined.contains_key(&SettingKey::new(name))
    }

    pub(crate) fn current_values(&self) -> &SettingValues {
        &self.current
    }

    pub(crate) fn has_changes_to_end(&self) -> bool {
        self.memo_before_statement.is_some()
            || self.memo_before_transaction.is_some()
            || self.current.has_transaction_values
    }

    pub(crate) fn save_memo_before_change(
        &mut self,
        statement_running: bool,
        in_transaction: bool,
    ) {
        if statement_running && self.memo_before_statement.is_none() {
            self.memo_before_statement = Some(self.current.clone());
        }
        if (statement_running || in_transaction) && self.memo_before_transaction.is_none() {
            self.memo_before_transaction = Some(self.current.clone());
        }
    }

    pub(crate) fn set_search_path(
        &mut self,
        search_path: Option<Vec<String>>,
        scope: SettingScope,
    ) {
        self.current.search_path.set(search_path, scope);
        self.current.has_transaction_values |= scope == SettingScope::Transaction;
    }

    pub(crate) fn set_role(&mut self, role: Option<SetRole>, scope: SettingScope) {
        self.current.role.set(role, scope);
        self.current.has_transaction_values |= scope == SettingScope::Transaction;
    }

    /// Sets the built-in `parameter` to `value`, or resets it when `value` is
    /// `None`. `search_path` has no reset value, so a reset leaves it with no
    /// value and it shows as its default.
    pub(crate) fn set(
        &mut self,
        parameter: &BuiltInParameter,
        value: Option<String>,
        scope: SettingScope,
    ) {
        self.set_named(parameter.name, value, scope);
    }

    /// Sets the custom parameter `name` to `value`, or resets it when `value`
    /// is `None`. A custom parameter without a reset value then shows as
    /// empty.
    pub(crate) fn set_custom(
        &mut self,
        name: CustomName<'_>,
        value: Option<String>,
        scope: SettingScope,
    ) {
        self.set_named(name.as_str(), value, scope);
    }

    fn set_named(&mut self, name: &str, value: Option<String>, scope: SettingScope) {
        let key = SettingKey::new(name);
        self.names_as_defined
            .entry(key.clone())
            .or_insert_with(|| name.to_owned());
        let value = value.or_else(|| self.reset_values.get(&key).cloned());
        self.current.named.entry(key).or_default().set(value, scope);
        self.current.has_transaction_values |= scope == SettingScope::Transaction;
    }

    /// Gives the built-in `parameter` the value it starts the session with,
    /// which RESET returns it to.
    fn define(&mut self, parameter: &BuiltInParameter, value: String) {
        self.define_named(parameter.name, value);
    }

    /// Gives the custom parameter `name` the value it starts the session
    /// with, which RESET returns it to.
    fn define_custom(&mut self, name: CustomName<'_>, value: String) {
        self.define_named(name.as_str(), value);
    }

    fn define_named(&mut self, name: &str, value: String) {
        self.reset_values.insert(SettingKey::new(name), value);
        self.set_named(name, None, SettingScope::Session);
    }

    /// Resets every named parameter for the session.
    pub(crate) fn reset_all(&mut self) {
        let names: Vec<String> = self.names_as_defined.values().cloned().collect();
        for name in names {
            self.set_named(&name, None, SettingScope::Session);
        }
    }

    fn end_statement(
        &mut self,
        statement_failed: bool,
        transaction_ended: bool,
    ) -> Option<CoreValues> {
        let memo_before_statement = self.memo_before_statement.take();
        let memo_to_restore = if transaction_ended {
            self.memo_before_transaction
                .take()
                .or(memo_before_statement)
        } else {
            memo_before_statement
        };
        self.change_current_values(|current| {
            if let Some(saved) = memo_to_restore.filter(|_| statement_failed) {
                current.restore(saved);
            }
            if transaction_ended {
                current.end_transaction();
            }
        })
    }

    fn rollback_transaction(&mut self) -> Option<CoreValues> {
        self.memo_before_statement = None;
        let memo_before_transaction = self.memo_before_transaction.take();
        self.change_current_values(|current| {
            if let Some(saved) = memo_before_transaction {
                current.restore(saved);
            }
            current.end_transaction();
        })
    }

    fn rollback_to_savepoint(&mut self, saved: SettingValues) -> Option<CoreValues> {
        self.change_current_values(|current| current.restore(saved))
    }

    /// Applies `change` and returns the values core keeps if any of them
    /// changed.
    fn change_current_values(
        &mut self,
        change: impl FnOnce(&mut SettingValues),
    ) -> Option<CoreValues> {
        let before = self.current.core_values();
        change(&mut self.current);
        let after = self.current.core_values();
        (after != before).then_some(after)
    }
}

impl SettingValues {
    fn core_values(&self) -> CoreValues {
        CoreValues {
            search_path: self.search_path.current().clone(),
            role: self.role.current().as_ref().map(|role| role.id),
        }
    }

    fn restore(&mut self, mut saved: SettingValues) {
        self.search_path = saved.search_path;
        self.role = saved.role;
        self.has_transaction_values = saved.has_transaction_values;
        for (name, setting) in &mut self.named {
            *setting = saved.named.remove(name).unwrap_or_default();
        }
    }

    fn end_transaction(&mut self) {
        self.search_path.end_transaction();
        self.role.end_transaction();
        for setting in self.named.values_mut() {
            setting.end_transaction();
        }
        self.has_transaction_values = false;
    }
}

impl<T> Setting<T> {
    fn set(&mut self, value: T, scope: SettingScope) {
        match scope {
            SettingScope::Session => {
                self.session_value = value;
                self.transaction_value = None;
            }
            SettingScope::Transaction => self.transaction_value = Some(value),
        }
    }

    fn current(&self) -> &T {
        self.transaction_value
            .as_ref()
            .unwrap_or(&self.session_value)
    }

    fn end_transaction(&mut self) {
        self.transaction_value = None;
    }
}

pub(crate) enum Parameter<'a> {
    BuiltIn(&'static BuiltInParameter),
    Custom(&'a str),
}

/// Finds the parameter `name`. A name with a dot is a custom parameter, as in
/// PostgreSQL. Any other name that is not built in is refused.
pub(crate) fn find_parameter(name: &str) -> turso_core::Result<Parameter<'_>> {
    if let Some(parameter) = PARAMETERS
        .iter()
        .find(|parameter| parameter.name.eq_ignore_ascii_case(name))
    {
        Ok(Parameter::BuiltIn(parameter))
    } else if name.contains('.') {
        Ok(Parameter::Custom(name))
    } else {
        Err(feature_not_supported(format!(
            "parameter \"{name}\" is not supported"
        )))
    }
}

/// Gives a setting the value of one parameter of a client's startup message.
/// `user` and `database` are not settings, so they set nothing.
fn define_startup_value(
    settings: &mut ConnectionSettings,
    name: &str,
    value: &str,
) -> turso_core::Result<()> {
    match name {
        // tursopg serves one database as one built-in user.
        "user" | "database" => Ok(()),
        "options" => Err(feature_not_supported(
            "the startup parameter \"options\" is not supported; use SET after connecting",
        )),
        "replication" => Err(feature_not_supported(
            "replication connections are not supported",
        )),
        _ if name.starts_with("_pq_.") => Err(feature_not_supported(format!(
            "protocol extension \"{name}\" is not supported"
        ))),
        _ => match find_parameter(name)? {
            Parameter::BuiltIn(parameter) => {
                settings.define(parameter, parameter.startup_value(value)?);
                Ok(())
            }
            Parameter::Custom(name) => {
                settings.define_custom(CustomName::new(name)?, value.to_owned());
                Ok(())
            }
        },
    }
}

/// Sets the parameter `name` to `value`, or resets it when `value` is `None`,
/// as set_config does, and returns the value SHOW reports afterwards.
pub(crate) fn set_parameter(
    conn: &Connection,
    name: &str,
    value: Option<&str>,
    scope: SettingScope,
) -> turso_core::Result<String> {
    match find_parameter(name)? {
        Parameter::BuiltIn(parameter) => parameter.set(conn, value, scope),
        Parameter::Custom(name) => crate::session::set_custom_parameter(conn, name, value, scope),
    }
}

/// The parameters whose values a server sends its client when the session
/// starts.
pub(crate) fn reported_parameters(conn: &Connection) -> HashMap<String, String> {
    PARAMETERS
        .iter()
        .filter(|parameter| parameter.reported)
        .map(|parameter| (parameter.name.to_owned(), parameter.get(conn)))
        .collect()
}

pub(crate) fn unrecognized_parameter(name: &str) -> turso_core::LimboError {
    crate::error::sqlstate_error(
        turso_pg_parser::sqlstate::SQLSTATE_UNDEFINED_OBJECT,
        format!("unrecognized configuration parameter \"{name}\""),
    )
}

fn feature_not_supported(message: impl Into<String>) -> turso_core::LimboError {
    crate::error::sqlstate_error(
        turso_pg_parser::sqlstate::SQLSTATE_FEATURE_NOT_SUPPORTED,
        message,
    )
}

pub(crate) struct BuiltInParameter {
    name: &'static str,
    /// Whether a server sends the value to its client when the session
    /// starts, as PostgreSQL 17 does for the parameters it marks GUC_REPORT.
    reported: bool,
    pub(crate) kind: Kind,
}

pub(crate) enum Kind {
    SearchPath,
    Role,
    /// A value the session keeps as text. It accepts only the values that
    /// tursopg implements.
    Stored {
        default: &'static str,
        accepts: Accepts,
    },
}

impl BuiltInParameter {
    /// Sets the parameter to `value`, or resets it when `value` is `None`,
    /// and returns the value SHOW reports afterwards.
    pub(crate) fn set(
        &self,
        conn: &Connection,
        value: Option<&str>,
        scope: SettingScope,
    ) -> turso_core::Result<String> {
        match &self.kind {
            Kind::SearchPath => crate::session::set_search_path(conn, value, scope),
            Kind::Role => crate::session::set_role(conn, value, scope),
            Kind::Stored { default, accepts } => {
                let value = value
                    .map(|value| self.stored_value(default, accepts, value))
                    .transpose()?;
                session_state(conn).mutate(conn, |settings| {
                    settings.set(self, value, scope);
                });
                Ok(self.get(conn))
            }
        }
    }

    fn startup_value(&self, value: &str) -> turso_core::Result<String> {
        match &self.kind {
            // search_path and role keep no startup value for RESET to return to.
            Kind::SearchPath | Kind::Role => Err(feature_not_supported(format!(
                "the startup parameter \"{}\" is not supported; use SET after connecting",
                self.name
            ))),
            Kind::Stored { default, accepts } => self.stored_value(default, accepts, value),
        }
    }

    pub(crate) fn get(&self, conn: &Connection) -> String {
        match &self.kind {
            Kind::SearchPath => crate::session::search_path_setting(conn)
                .unwrap_or_else(|| crate::session::DEFAULT_SEARCH_PATH.to_owned()),
            Kind::Role => session_state(conn)
                .role_name()
                .unwrap_or_else(|| "none".to_owned()),
            Kind::Stored { .. } => session_state(conn)
                .setting(self.name)
                .expect("a stored parameter has a value from the start of the session"),
        }
    }

    pub(crate) fn name(&self) -> &'static str {
        self.name
    }

    pub(crate) fn accepts_list(&self) -> bool {
        matches!(
            self.kind,
            Kind::Stored {
                accepts: Accepts::DateStyle,
                ..
            }
        )
    }

    /// The value SHOW reports after the parameter is set to `value`.
    fn stored_value(
        &self,
        default: &'static str,
        accepts: &Accepts,
        value: &str,
    ) -> turso_core::Result<String> {
        match accepts.stored_value(value, default) {
            Some(stored) => Ok(stored.to_owned()),
            None => Err(feature_not_supported(format!(
                "parameter \"{}\" does not support value \"{value}\"; supported: {}",
                self.name,
                accepts.supported(default)
            ))),
        }
    }
}

pub(crate) enum Accepts {
    /// The default, in any ASCII case.
    DefaultAnyCase,
    /// The default, exactly.
    Default,
    /// A PostgreSQL spelling of this boolean.
    Bool(bool),
    /// The ISO output style and the MDY input order.
    DateStyle,
    /// A name of the UTF8 encoding.
    ClientEncoding,
    /// 1, 2 or 3. For each of them PostgreSQL 12 and later print the
    /// shortest text that reads back as the same float, as tursopg does.
    FloatDigits,
    /// One of the names of UTC whose output is the same in PostgreSQL.
    Utc,
    /// Printable ASCII of at most 63 bytes, which PostgreSQL stores as given.
    PrintableAscii,
}

impl Accepts {
    /// The value SHOW reports after the parameter is set to `value`, or
    /// `None` if tursopg does not support `value`.
    fn stored_value<'a>(&self, value: &'a str, default: &'static str) -> Option<&'a str> {
        match self {
            Self::DefaultAnyCase => value.eq_ignore_ascii_case(default).then_some(default),
            Self::Default => (value == default).then_some(default),
            Self::Bool(expected) => {
                (crate::functions::parse_bool_setting(value) == Some(*expected)).then_some(default)
            }
            // PostgreSQL reads each comma-separated word as an output style or
            // an input order; US, NONEURO and NONEUROPEAN are names of MDY.
            Self::DateStyle => value
                .split(',')
                .map(str::trim)
                .all(|word| {
                    ["iso", "mdy", "us", "noneuro", "noneuropean"]
                        .iter()
                        .any(|name| word.eq_ignore_ascii_case(name))
                })
                .then_some(default),
            // PostgreSQL skips every character of an encoding name that is
            // not a letter or digit, so `'utf-8'` with its quotes names UTF8.
            // PostgreSQL reads only the first 63 bytes of a longer name, so tursopg
            // refuses longer names.
            Self::ClientEncoding => {
                if value.len() > 63 {
                    return None;
                }
                let name: String = value
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .map(|c| c.to_ascii_lowercase())
                    .collect();
                matches!(name.as_str(), "utf8" | "unicode").then_some(default)
            }
            Self::FloatDigits => matches!(value, "1" | "2" | "3").then_some(value),
            Self::Utc => ["UTC", "Etc/UTC"]
                .into_iter()
                .find(|zone| zone.eq_ignore_ascii_case(value)),
            Self::PrintableAscii => (value.len() <= 63
                && value.bytes().all(|byte| matches!(byte, b' '..=b'~')))
            .then_some(value),
        }
    }

    fn supported(&self, default: &str) -> String {
        match self {
            Self::DefaultAnyCase
            | Self::Default
            | Self::Bool(_)
            | Self::DateStyle
            | Self::ClientEncoding => format!("\"{default}\""),
            Self::FloatDigits => "\"1\", \"2\", \"3\"".to_owned(),
            Self::Utc => "\"UTC\", \"Etc/UTC\"".to_owned(),
            Self::PrintableAscii => "printable ASCII text of at most 63 bytes".to_owned(),
        }
    }
}

/// Every parameter that tursopg knows besides custom ones. Many stored
/// parameters exist for the SET commands that pg_dump and drivers send; they
/// accept only the modes tursopg works in, since tursopg implements no
/// timeouts, row-level security bypass or other date formats.
static PARAMETERS: &[BuiltInParameter] = &[
    SEARCH_PATH,
    parameter("role", Kind::Role),
    reported("application_name", stored("", Accepts::PrintableAscii)),
    reported("client_encoding", stored("UTF8", Accepts::ClientEncoding)),
    reported("DateStyle", stored("ISO, MDY", Accepts::DateStyle)),
    reported("IntervalStyle", stored("postgres", Accepts::DefaultAnyCase)),
    reported(
        "standard_conforming_strings",
        stored("on", Accepts::Bool(true)),
    ),
    reported("TimeZone", stored("Etc/UTC", Accepts::Utc)),
    parameter("extra_float_digits", stored("1", Accepts::FloatDigits)),
    parameter("statement_timeout", stored("0", Accepts::Default)),
    parameter("lock_timeout", stored("0", Accepts::Default)),
    parameter(
        "idle_in_transaction_session_timeout",
        stored("0", Accepts::Default),
    ),
    parameter("transaction_timeout", stored("0", Accepts::Default)),
    parameter("row_security", stored("off", Accepts::Bool(false))),
    parameter("quote_all_identifiers", stored("off", Accepts::Bool(false))),
    parameter("check_function_bodies", stored("off", Accepts::Bool(false))),
    parameter("synchronize_seqscans", stored("off", Accepts::Bool(false))),
    parameter("default_tablespace", stored("", Accepts::Default)),
    parameter(
        "default_table_access_method",
        stored("heap", Accepts::Default),
    ),
    parameter("xmloption", stored("content", Accepts::DefaultAnyCase)),
    parameter(
        "client_min_messages",
        stored("warning", Accepts::DefaultAnyCase),
    ),
];

pub(crate) const SEARCH_PATH: BuiltInParameter = parameter("search_path", Kind::SearchPath);

const fn parameter(name: &'static str, kind: Kind) -> BuiltInParameter {
    BuiltInParameter {
        name,
        reported: false,
        kind,
    }
}

const fn reported(name: &'static str, kind: Kind) -> BuiltInParameter {
    BuiltInParameter {
        name,
        reported: true,
        kind,
    }
}

const fn stored(default: &'static str, accepts: Accepts) -> Kind {
    Kind::Stored { default, accepts }
}

#[cfg(test)]
mod tests {
    use super::{find_parameter, Kind, Parameter, PARAMETERS};

    #[test]
    fn config_parameter_parses_sql_names() {
        for name in ["search_path", "SEARCH_PATH", "SeArCh_PaTh"] {
            assert!(
                matches!(
                    find_parameter(name),
                    Ok(Parameter::BuiltIn(parameter)) if matches!(parameter.kind, Kind::SearchPath)
                ),
                "{name}"
            );
        }
        for parameter in PARAMETERS {
            for name in [
                parameter.name.to_ascii_lowercase(),
                parameter.name.to_ascii_uppercase(),
            ] {
                assert!(
                    matches!(
                        find_parameter(&name),
                        Ok(Parameter::BuiltIn(found)) if std::ptr::eq(found, parameter)
                    ),
                    "{name}"
                );
            }
        }
        for name in ["", "SearchPath", "search_path "] {
            assert!(find_parameter(name).is_err(), "{name}");
        }
    }
}
