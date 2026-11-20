//! Lint warning names and option masks (`LinterConfig.h` / `LinterConfig.cpp`).

/// Lint warning codes are indices into [`WARNING_NAMES`]; 0 is `Unknown`.
pub type LintCode = u32;

pub const WARNING_NAMES: [&str; 30] = [
    "Unknown",
    "UnknownGlobal",
    "DeprecatedGlobal",
    "GlobalUsedAsLocal",
    "LocalShadow",
    "SameLineStatement",
    "MultiLineStatement",
    "LocalUnused",
    "FunctionUnused",
    "ImportUnused",
    "BuiltinGlobalWrite",
    "PlaceholderRead",
    "UnreachableCode",
    "UnknownType",
    "ForRange",
    "UnbalancedAssignment",
    "ImplicitReturn",
    "DuplicateLocal",
    "FormatString",
    "TableLiteral",
    "UninitializedLocal",
    "DuplicateFunction",
    "DeprecatedApi",
    "TableOperations",
    "DuplicateCondition",
    "MisleadingAndOr",
    "CommentDirective",
    "IntegerParsing",
    "ComparisonPrecedence",
    "RedundantNativeAttribute",
];

pub fn get_name(code: LintCode) -> &'static str {
    WARNING_NAMES[code as usize]
}

/// `LintWarning::parseName`: 0 (`Unknown`) when the name is not a lint.
pub fn parse_name(name: &str) -> LintCode {
    WARNING_NAMES.iter().position(|&n| n == name).unwrap_or(0) as LintCode
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LintOptions {
    pub warning_mask: u64,
}

impl LintOptions {
    pub fn enable_warning(&mut self, code: LintCode) {
        self.warning_mask |= 1u64 << code;
    }
    pub fn disable_warning(&mut self, code: LintCode) {
        self.warning_mask &= !(1u64 << code);
    }
    pub fn is_enabled(&self, code: LintCode) -> bool {
        self.warning_mask & (1u64 << code) != 0
    }
    /// By default all warnings are enabled.
    pub fn set_defaults(&mut self) {
        self.warning_mask = !0u64;
    }
}
