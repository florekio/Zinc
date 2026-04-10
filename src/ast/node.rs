use crate::ast::span::Span;
use crate::util::interner::StringId;

/// Root AST node: a complete program.
#[derive(Debug)]
pub struct Program {
    pub body: Vec<Statement>,
    pub source_type: SourceType,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceType {
    Script,
    Module,
}

// ============================================================
// Statements
// ============================================================

#[derive(Debug)]
pub enum Statement {
    Block(BlockStatement),
    Variable(VariableDeclaration),
    Empty(Span),
    Expression(ExpressionStatement),
    If(Box<IfStatement>),
    While(Box<WhileStatement>),
    DoWhile(Box<DoWhileStatement>),
    For(Box<ForStatement>),
    ForIn(Box<ForInStatement>),
    ForOf(Box<ForOfStatement>),
    Switch(Box<SwitchStatement>),
    Return(ReturnStatement),
    Break(BreakStatement),
    Continue(ContinueStatement),
    Throw(ThrowStatement),
    Try(Box<TryStatement>),
    With(Box<WithStatement>),
    Labeled(Box<LabeledStatement>),
    Debugger(Span),
    Function(FunctionDeclaration),
    Class(ClassDeclaration),
    Import(ImportDeclaration),
    Export(Box<ExportDeclaration>),
}

#[derive(Debug)]
pub struct BlockStatement {
    pub body: Vec<Statement>,
    pub span: Span,
}

#[derive(Debug)]
pub struct ExpressionStatement {
    pub expression: Expression,
    pub span: Span,
}

#[derive(Debug)]
pub struct IfStatement {
    pub test: Expression,
    pub consequent: Statement,
    pub alternate: Option<Statement>,
    pub span: Span,
}

#[derive(Debug)]
pub struct WhileStatement {
    pub test: Expression,
    pub body: Statement,
    pub span: Span,
}

#[derive(Debug)]
pub struct DoWhileStatement {
    pub body: Statement,
    pub test: Expression,
    pub span: Span,
}

#[derive(Debug)]
pub struct ForStatement {
    pub init: Option<ForInit>,
    pub test: Option<Expression>,
    pub update: Option<Expression>,
    pub body: Statement,
    pub span: Span,
}

#[derive(Debug)]
pub enum ForInit {
    Variable(VariableDeclaration),
    Expression(Expression),
}

#[derive(Debug)]
pub struct ForInStatement {
    pub left: ForInOfLeft,
    pub right: Expression,
    pub body: Statement,
    pub span: Span,
}

#[derive(Debug)]
pub struct ForOfStatement {
    pub left: ForInOfLeft,
    pub right: Expression,
    pub body: Statement,
    pub is_await: bool,
    pub span: Span,
}

#[derive(Debug)]
pub enum ForInOfLeft {
    Variable(VariableDeclaration),
    Pattern(Pattern),
}

#[derive(Debug)]
pub struct SwitchStatement {
    pub discriminant: Expression,
    pub cases: Vec<SwitchCase>,
    pub span: Span,
}

#[derive(Debug)]
pub struct SwitchCase {
    /// None for `default:`
    pub test: Option<Expression>,
    pub consequent: Vec<Statement>,
    pub span: Span,
}

#[derive(Debug)]
pub struct ReturnStatement {
    pub argument: Option<Expression>,
    pub span: Span,
}

#[derive(Debug)]
pub struct BreakStatement {
    pub label: Option<StringId>,
    pub span: Span,
}

#[derive(Debug)]
pub struct ContinueStatement {
    pub label: Option<StringId>,
    pub span: Span,
}

#[derive(Debug)]
pub struct ThrowStatement {
    pub argument: Expression,
    pub span: Span,
}

#[derive(Debug)]
pub struct TryStatement {
    pub block: BlockStatement,
    pub handler: Option<CatchClause>,
    pub finalizer: Option<BlockStatement>,
    pub span: Span,
}

#[derive(Debug)]
pub struct CatchClause {
    pub param: Option<Pattern>,
    pub body: BlockStatement,
    pub span: Span,
}

#[derive(Debug)]
pub struct WithStatement {
    pub object: Expression,
    pub body: Statement,
    pub span: Span,
}

#[derive(Debug)]
pub struct LabeledStatement {
    pub label: StringId,
    pub body: Statement,
    pub span: Span,
}

// ============================================================
// Declarations
// ============================================================

#[derive(Debug)]
pub struct VariableDeclaration {
    pub kind: VarKind,
    pub declarations: Vec<VariableDeclarator>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    Var,
    Let,
    Const,
}

#[derive(Debug)]
pub struct VariableDeclarator {
    pub id: Pattern,
    pub init: Option<Expression>,
    pub span: Span,
}

#[derive(Debug)]
pub struct FunctionDeclaration {
    pub id: Option<StringId>,
    pub params: Vec<Pattern>,
    pub body: BlockStatement,
    pub is_async: bool,
    pub is_generator: bool,
    pub span: Span,
}

#[derive(Debug)]
pub struct ClassDeclaration {
    pub id: Option<StringId>,
    pub super_class: Option<Expression>,
    pub body: ClassBody,
    pub span: Span,
}

#[derive(Debug)]
pub struct ClassBody {
    pub body: Vec<ClassMember>,
    pub span: Span,
}

#[derive(Debug)]
pub enum ClassMember {
    Method(MethodDefinition),
    Property(ClassProperty),
    StaticBlock(BlockStatement),
}

#[derive(Debug)]
pub struct MethodDefinition {
    pub key: PropertyKey,
    pub value: Expression, // should be FunctionExpression
    pub kind: MethodKind,
    pub is_static: bool,
    pub computed: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    Method,
    Get,
    Set,
    Constructor,
}

#[derive(Debug)]
pub struct ClassProperty {
    pub key: PropertyKey,
    pub value: Option<Expression>,
    pub is_static: bool,
    pub computed: bool,
    pub span: Span,
}

// ============================================================
// Expressions
// ============================================================

#[derive(Debug)]
pub enum Expression {
    // Literals
    NumberLiteral(NumberLiteral),
    StringLiteral(StringLiteral),
    BooleanLiteral(BooleanLiteral),
    NullLiteral(Span),
    RegExpLiteral(RegExpLiteral),
    TemplateLiteral(TemplateLiteral),

    // Identifiers / this
    Identifier(Identifier),
    This(Span),

    // Compound
    Array(ArrayExpression),
    Object(ObjectExpression),
    Function(Box<FunctionExpression>),
    ArrowFunction(Box<ArrowFunctionExpression>),
    Class(Box<ClassExpression>),

    // Operations
    Unary(Box<UnaryExpression>),
    Update(Box<UpdateExpression>),
    Binary(Box<BinaryExpression>),
    Logical(Box<LogicalExpression>),
    Conditional(Box<ConditionalExpression>),
    Assignment(Box<AssignmentExpression>),
    Sequence(SequenceExpression),

    // Member / Call
    Member(Box<MemberExpression>),
    Call(Box<CallExpression>),
    New(Box<NewExpression>),
    TaggedTemplate(Box<TaggedTemplateExpression>),
    OptionalChain(Box<OptionalChainExpression>),

    // Special
    Spread(Box<SpreadElement>),
    Yield(Box<YieldExpression>),
    Await(Box<AwaitExpression>),
    MetaProperty(MetaProperty),
    Import(Box<ImportExpression>),
    Super(Span),
}

#[derive(Debug)]
pub struct NumberLiteral {
    pub value: f64,
    pub span: Span,
}

#[derive(Debug)]
pub struct StringLiteral {
    pub value: StringId,
    pub span: Span,
}

#[derive(Debug)]
pub struct BooleanLiteral {
    pub value: bool,
    pub span: Span,
}

#[derive(Debug)]
pub struct RegExpLiteral {
    pub pattern: StringId,
    pub flags: StringId,
    pub span: Span,
}

#[derive(Debug)]
pub struct TemplateLiteral {
    pub quasis: Vec<TemplateElement>,
    pub expressions: Vec<Expression>,
    pub span: Span,
}

#[derive(Debug)]
pub struct TemplateElement {
    pub raw: StringId,
    pub cooked: Option<StringId>,
    pub tail: bool,
    pub span: Span,
}

#[derive(Debug)]
pub struct Identifier {
    pub name: StringId,
    pub span: Span,
}

#[derive(Debug)]
pub struct ArrayExpression {
    /// None elements represent elision (holes): [1,,3]
    pub elements: Vec<Option<Expression>>,
    pub span: Span,
}

#[derive(Debug)]
pub struct ObjectExpression {
    pub properties: Vec<ObjectProperty>,
    pub span: Span,
}

#[derive(Debug)]
pub enum ObjectProperty {
    Property(Property),
    SpreadElement(SpreadElement),
}

#[derive(Debug)]
pub struct Property {
    pub key: PropertyKey,
    pub value: Expression,
    pub kind: PropertyKindVal,
    pub shorthand: bool,
    pub computed: bool,
    pub method: bool,
    pub span: Span,
}

#[derive(Debug)]
pub enum PropertyKey {
    Identifier(StringId),
    StringLiteral(StringId),
    NumberLiteral(f64),
    Computed(Box<Expression>),
    Private(StringId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyKindVal {
    Init,
    Get,
    Set,
}

#[derive(Debug)]
pub struct FunctionExpression {
    pub id: Option<StringId>,
    pub params: Vec<Pattern>,
    pub body: BlockStatement,
    pub is_async: bool,
    pub is_generator: bool,
    pub span: Span,
}

#[derive(Debug)]
pub struct ArrowFunctionExpression {
    pub params: Vec<Pattern>,
    pub body: ArrowBody,
    pub is_async: bool,
    pub span: Span,
}

#[derive(Debug)]
pub enum ArrowBody {
    Expression(Expression),
    Block(BlockStatement),
}

#[derive(Debug)]
pub struct ClassExpression {
    pub id: Option<StringId>,
    pub super_class: Option<Expression>,
    pub body: ClassBody,
    pub span: Span,
}

#[derive(Debug)]
pub struct UnaryExpression {
    pub operator: UnaryOperator,
    pub argument: Expression,
    pub prefix: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOperator {
    Minus,
    Plus,
    Not,
    BitNot,
    TypeOf,
    Void,
    Delete,
}

#[derive(Debug)]
pub struct UpdateExpression {
    pub operator: UpdateOperator,
    pub argument: Expression,
    pub prefix: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOperator {
    Increment,
    Decrement,
}

#[derive(Debug)]
pub struct BinaryExpression {
    pub operator: BinaryOperator,
    pub left: Expression,
    pub right: Expression,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Exp,
    EqEq,
    NotEq,
    StrictEq,
    StrictNotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    UShr,
    In,
    InstanceOf,
}

#[derive(Debug)]
pub struct LogicalExpression {
    pub operator: LogicalOperator,
    pub left: Expression,
    pub right: Expression,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOperator {
    And,
    Or,
    NullishCoalescing,
}

#[derive(Debug)]
pub struct ConditionalExpression {
    pub test: Expression,
    pub consequent: Expression,
    pub alternate: Expression,
    pub span: Span,
}

#[derive(Debug)]
pub struct AssignmentExpression {
    pub operator: AssignmentOperator,
    pub left: AssignmentTarget,
    pub right: Expression,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignmentOperator {
    Assign,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    RemAssign,
    ExpAssign,
    BitAndAssign,
    BitOrAssign,
    BitXorAssign,
    ShlAssign,
    ShrAssign,
    UShrAssign,
    AndAssign,
    OrAssign,
    NullishAssign,
}

#[derive(Debug)]
pub enum AssignmentTarget {
    Identifier(Identifier),
    Member(Box<MemberExpression>),
    Pattern(Pattern),
}

#[derive(Debug)]
pub struct SequenceExpression {
    pub expressions: Vec<Expression>,
    pub span: Span,
}

#[derive(Debug)]
pub struct MemberExpression {
    pub object: Expression,
    pub property: MemberProperty,
    pub computed: bool,
    pub span: Span,
}

#[derive(Debug)]
pub enum MemberProperty {
    Identifier(StringId),
    Expression(Expression),
    PrivateIdentifier(StringId),
}

#[derive(Debug)]
pub struct CallExpression {
    pub callee: Expression,
    pub arguments: Vec<Expression>,
    pub span: Span,
}

#[derive(Debug)]
pub struct NewExpression {
    pub callee: Expression,
    pub arguments: Vec<Expression>,
    pub span: Span,
}

#[derive(Debug)]
pub struct TaggedTemplateExpression {
    pub tag: Expression,
    pub quasi: TemplateLiteral,
    pub span: Span,
}

#[derive(Debug)]
pub struct OptionalChainExpression {
    pub base: Expression,
    pub chain: Vec<OptionalChainElement>,
    pub span: Span,
}

#[derive(Debug)]
pub enum OptionalChainElement {
    Member {
        property: MemberProperty,
        computed: bool,
        optional: bool,
    },
    Call {
        arguments: Vec<Expression>,
        optional: bool,
    },
}

#[derive(Debug)]
pub struct SpreadElement {
    pub argument: Expression,
    pub span: Span,
}

#[derive(Debug)]
pub struct YieldExpression {
    pub argument: Option<Expression>,
    pub delegate: bool,
    pub span: Span,
}

#[derive(Debug)]
pub struct AwaitExpression {
    pub argument: Expression,
    pub span: Span,
}

#[derive(Debug)]
pub struct MetaProperty {
    pub meta: StringId,
    pub property: StringId,
    pub span: Span,
}

#[derive(Debug)]
pub struct ImportExpression {
    pub source: Expression,
    pub span: Span,
}

// ============================================================
// Patterns (destructuring)
// ============================================================

#[derive(Debug)]
pub enum Pattern {
    Identifier(Identifier),
    Array(ArrayPattern),
    Object(ObjectPattern),
    Assignment(Box<AssignmentPattern>),
    Rest(Box<RestElement>),
}

#[derive(Debug)]
pub struct ArrayPattern {
    /// None elements represent holes
    pub elements: Vec<Option<Pattern>>,
    pub span: Span,
}

#[derive(Debug)]
pub struct ObjectPattern {
    pub properties: Vec<ObjectPatternProperty>,
    pub span: Span,
}

#[derive(Debug)]
pub enum ObjectPatternProperty {
    Property {
        key: PropertyKey,
        value: Pattern,
        computed: bool,
        shorthand: bool,
        span: Span,
    },
    Rest(RestElement),
}

#[derive(Debug)]
pub struct AssignmentPattern {
    pub left: Pattern,
    pub right: Expression,
    pub span: Span,
}

#[derive(Debug)]
pub struct RestElement {
    pub argument: Pattern,
    pub span: Span,
}

// ============================================================
// Modules (import/export)
// ============================================================

#[derive(Debug)]
pub enum ImportDeclaration {
    /// import x from 'mod'; import {a, b} from 'mod'; etc.
    Standard {
        specifiers: Vec<ImportSpecifier>,
        source: StringId,
        span: Span,
    },
}

#[derive(Debug)]
pub enum ImportSpecifier {
    /// import x from 'mod'
    Default { local: StringId, span: Span },
    /// import { x } from 'mod' or import { x as y } from 'mod'
    Named {
        imported: StringId,
        local: StringId,
        span: Span,
    },
    /// import * as x from 'mod'
    Namespace { local: StringId, span: Span },
}

#[derive(Debug)]
pub enum ExportDeclaration {
    /// export { x, y }
    Named {
        specifiers: Vec<ExportSpecifier>,
        source: Option<StringId>,
        span: Span,
    },
    /// export default expr
    Default {
        declaration: Expression,
        span: Span,
    },
    /// export var/let/const/function/class
    Declaration {
        declaration: Box<Statement>,
        span: Span,
    },
    /// export * from 'mod'
    All {
        source: StringId,
        exported: Option<StringId>,
        span: Span,
    },
}

#[derive(Debug)]
pub struct ExportSpecifier {
    pub local: StringId,
    pub exported: StringId,
    pub span: Span,
}
