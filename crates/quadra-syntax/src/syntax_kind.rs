//! Every token and node kind of the Quadra syntax tree.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
#[allow(non_camel_case_types)]
pub enum SyntaxKind {
    // ---- trivia -----------------------------------------------------------------------
    WHITESPACE = 0,
    COMMENT,

    // ---- layout -----------------------------------------------------------------------
    NEWLINE,
    INDENT,
    DEDENT,

    // ---- literals and names -----------------------------------------------------------
    IDENT,
    NUMBER,
    PERCENT,
    STRING,
    FSTRING,
    DATE,
    DATETIME,
    YEAR_MONTH,

    // ---- punctuation ------------------------------------------------------------------
    L_PAREN,
    R_PAREN,
    L_BRACK,
    R_BRACK,
    L_BRACE,
    R_BRACE,
    COMMA,
    COLON,
    DOT,
    DOT2,
    QUESTION_DOT,
    QUESTION2,
    QUESTION,
    ARROW,
    FAT_ARROW,
    EQ,
    COLON_EQ,
    PLUS_EQ,
    MINUS_EQ,
    EQ2,
    NEQ,
    LT,
    GT,
    LE,
    GE,
    PLUS,
    MINUS,
    STAR,
    SLASH,
    PIPE,
    /// `@` of `@name`, an app's parameter (`.qdi` files only)
    AT,

    // ---- reserved keywords ------------------------------------------------------------
    AND_KW,
    AS_KW,
    CONST_KW,
    ELSE_KW,
    ENTITY_KW,
    ENUM_KW,
    EXTEND_KW,
    FALSE_KW,
    FN_KW,
    FOR_KW,
    FROM_KW,
    IF_KW,
    IMPORT_KW,
    IN_KW,
    IS_KW,
    LEDGER_KW,
    LET_KW,
    MATCH_KW,
    NONE_KW,
    NOT_KW,
    OR_KW,
    RECORD_KW,
    RETURN_KW,
    SELF_KW,
    TRAIT_KW,
    TRUE_KW,
    WHEN_KW,
    WHERE_KW,

    /// An identifier used as a keyword in a position where it has a special meaning
    /// (`check`, `parent`, `role`, `test`…). Its text tells which one.
    CONTEXT_KW,

    ERROR_TOKEN,

    // ---- nodes ------------------------------------------------------------------------
    SOURCE_FILE,
    ERROR,

    NAME,
    PATH,
    PATH_SEGMENT,

    // top-level items
    IMPORT,
    ENTITY,
    ENUM,
    VARIANT_LIST,
    VARIANT,
    LEDGER,
    FN,
    CONST,
    SEED,
    REACTION,
    VIEW,
    VIEW_ITEM,
    REPORT,
    REPORT_ITEM,
    ROLE,
    RULE,
    METRIC,
    CONTROL,
    MIGRATION,
    CHANGE,
    FIXTURE,
    TEST,
    EVENT,
    OUTBOUND,
    /// `from json` after a record's name: the external format it is read from and written to
    FORMAT_CLAUSE,
    /// an option of the format: `sep=";"` in `from csv(sep=";")`
    FORMAT_OPTION,
    /// a line of policy at the top of an `outbound … as e:` block: `reply ErpAck`
    OUTBOUND_CLAUSE,
    /// `inbound ShopOrder via shop path "/orders" as m: ShopOrderDoc:` and its handler
    INBOUND,
    /// `endpoint GET "/price" via pos as q: PriceQuery -> PosPrice:` and its block
    ENDPOINT,
    /// `publish Product to shop as p:`: the state of records sent to a connection
    PUBLISH,
    /// in a test: `receive OrderIn "…"`, `deliver OrderPlaced reply …`, `expect sent …`
    TEST_STMT,

    // members
    MEMBER_LIST,
    /// `ns cbc "urn:…"` in a record `from xml`: a prefix of its paths
    NAMESPACE,
    IS_CLAUSE,
    FIELD,
    MODIFIER,
    MODIFIER_BLOCK,
    VALUE,
    DEFAULT_VALUE,
    CHECK,
    WARN,
    UNIQUE,
    FROZEN,
    LIVE,
    FLOW,
    TRANSITION,
    ACTION,
    POST,
    LOAD,
    META,
    NAME_LIST,

    // parameters
    PARAM_LIST,
    PARAM,
    RET_TYPE,

    // types
    TYPE_PATH,
    GENERIC_ARGS,
    TYPE_OPTIONAL,
    TYPE_LIST,
    TYPE_SET,
    TYPE_UNION,
    TYPE_ENUM,
    TYPE_PAREN,
    TYPE_TUPLE,
    FIELD_LIST,
    TUPLE_FIELD,

    // statements
    BLOCK,
    LET_STMT,
    IF_STMT,
    ELSE_BRANCH,
    FOR_STMT,
    MATCH_STMT,
    RETURN_STMT,
    KEYWORD_STMT,
    EXPECT_BLOCK,
    AS_STMT,
    ASSIGN_STMT,
    EXPR_STMT,
    MIGRATION_STEP,

    // expressions
    NAME_REF,
    /// `@name`: a parameter of an app (filters, selections), in `.qdi` files only
    PARAM_REF,
    LITERAL,
    QUANTITY,
    UNIT,
    BIN_EXPR,
    PREFIX_EXPR,
    IF_EXPR,
    FIELD_EXPR,
    CALL_EXPR,
    ARG_LIST,
    ARG,
    INDEX_EXPR,
    PAREN_EXPR,
    TUPLE_EXPR,
    LIST_EXPR,
    SET_EXPR,
    WHEN_EXPR,
    MATCH_EXPR,
    MATCH_ARM,
    WHEN_ARM,
    PATTERN,
    BLOCK_EXPR,
    AS_EXPR,
    FAIL_EXPR,
    NAMED_ITEM,
    LAMBDA_EXPR,

    #[doc(hidden)]
    __LAST,
}

use SyntaxKind::*;

impl SyntaxKind {
    pub fn is_trivia(self) -> bool {
        matches!(self, WHITESPACE | COMMENT)
    }

    pub fn is_keyword(self) -> bool {
        (AND_KW as u16..=WHERE_KW as u16).contains(&(self as u16)) || self == CONTEXT_KW
    }

    pub fn from_keyword(text: &str) -> Option<SyntaxKind> {
        Some(match text {
            "and" => AND_KW,
            "as" => AS_KW,
            "const" => CONST_KW,
            "else" => ELSE_KW,
            "entity" => ENTITY_KW,
            "enum" => ENUM_KW,
            "extend" => EXTEND_KW,
            "false" => FALSE_KW,
            "fn" => FN_KW,
            "for" => FOR_KW,
            "from" => FROM_KW,
            "if" => IF_KW,
            "import" => IMPORT_KW,
            "in" => IN_KW,
            "is" => IS_KW,
            "ledger" => LEDGER_KW,
            "let" => LET_KW,
            "match" => MATCH_KW,
            "none" => NONE_KW,
            "not" => NOT_KW,
            "or" => OR_KW,
            "record" => RECORD_KW,
            "return" => RETURN_KW,
            "self" => SELF_KW,
            "trait" => TRAIT_KW,
            "true" => TRUE_KW,
            "when" => WHEN_KW,
            "where" => WHERE_KW,
            _ => return None,
        })
    }

    /// Human readable description, used in error messages.
    pub fn describe(self) -> &'static str {
        match self {
            NEWLINE => "end of line",
            INDENT => "indented block",
            DEDENT => "end of block",
            IDENT => "identifier",
            NUMBER => "number",
            STRING => "string",
            L_PAREN => "`(`",
            R_PAREN => "`)`",
            L_BRACK => "`[`",
            R_BRACK => "`]`",
            L_BRACE => "`{`",
            R_BRACE => "`}`",
            COMMA => "`,`",
            COLON => "`:`",
            DOT => "`.`",
            ARROW => "`->`",
            FAT_ARROW => "`=>`",
            EQ => "`=`",
            COLON_EQ => "`:=`",
            IMPORT_KW => "`import`",
            IN_KW => "`in`",
            ELSE_KW => "`else`",
            AS_KW => "`as`",
            GT => "`>`",
            AT => "`@`",
            _ => "token",
        }
    }
}

impl From<SyntaxKind> for rowan::SyntaxKind {
    fn from(kind: SyntaxKind) -> Self {
        rowan::SyntaxKind(kind as u16)
    }
}

#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq, Hash)]
pub enum QuadraLanguage {}

impl rowan::Language for QuadraLanguage {
    type Kind = SyntaxKind;

    fn kind_from_raw(raw: rowan::SyntaxKind) -> SyntaxKind {
        assert!(raw.0 < __LAST as u16);
        // SAFETY: SyntaxKind is repr(u16) and the value is in range.
        unsafe { std::mem::transmute::<u16, SyntaxKind>(raw.0) }
    }

    fn kind_to_raw(kind: SyntaxKind) -> rowan::SyntaxKind {
        kind.into()
    }
}

pub type SyntaxNode = rowan::SyntaxNode<QuadraLanguage>;
pub type SyntaxToken = rowan::SyntaxToken<QuadraLanguage>;
pub type SyntaxElement = rowan::SyntaxElement<QuadraLanguage>;
