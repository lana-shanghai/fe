//! Fe semantic analysis.
//!
//! This library is used to analyze the semantics of a given Fe AST. It detects
//! any semantic errors within a given AST and produces a `Context` instance
//! that can be used to query contextual information attributed to AST nodes.

pub mod builtins;
pub mod errors;
pub mod namespace;
mod operations;
mod traversal;

use crate::errors::SemanticError;
use crate::namespace::events::EventDef;
use crate::namespace::scopes::{
    ContractFunctionDef,
    ContractScope,
    Shared,
};
use crate::namespace::types::{
    Contract,
    FixedSize,
    Struct,
    Type,
};
use builtins::GlobalMethod;
use fe_parser::ast as fe;
use fe_parser::node::NodeId;
use std::cell::RefCell;
use std::collections::{
    HashMap,
    HashSet,
};
use std::rc::Rc;

/// Indicates where an expression is stored.
#[derive(Clone, Debug, PartialEq)]
pub enum Location {
    /// A storage value may not have a nonce known at compile time, so it is
    /// optional.
    Storage {
        nonce: Option<usize>,
    },
    Memory,
    Value,
}

impl Location {
    /// The expected location of a value with the given type when being
    /// assigned, returned, or passed.
    pub fn assign_location(typ: Type) -> Result<Self, SemanticError> {
        match typ {
            Type::Base(_) => Ok(Location::Value),
            Type::Contract(_) => Ok(Location::Value),
            Type::Array(_) => Ok(Location::Memory),
            Type::Tuple(_) => Ok(Location::Memory),
            Type::String(_) => Ok(Location::Memory),
            Type::Struct(_) => Ok(Location::Memory),
            Type::Map(_) => Err(SemanticError::cannot_move()),
        }
    }
}

/// Contains contextual information relating to a contract AST node.
#[derive(Clone, Debug, PartialEq)]
pub struct ContractAttributes {
    /// Public functions that have been defined by the user.
    pub public_functions: Vec<FunctionAttributes>,
    /// An init function that has been defined by the user.
    pub init_function: Option<FunctionAttributes>,
    /// Events that have been defined by the user.
    pub events: Vec<EventDef>,
    /// Static strings that the contract defines
    pub string_literals: HashSet<String>,
    /// Structs that have been defined by the user
    pub structs: Vec<Struct>,
    /// External contracts that may be called from within this contract.
    pub external_contracts: Vec<Contract>,
    /// Names of contracts that have been created inside of this contract.
    pub created_contracts: HashSet<String>,
}

impl From<Shared<ContractScope>> for ContractAttributes {
    fn from(scope: Shared<ContractScope>) -> Self {
        let mut public_functions = vec![];
        let mut init_function = None;

        for (name, def) in scope.borrow().function_defs.iter() {
            if !def.is_public {
                continue;
            }

            if name != "__init__" {
                public_functions.push(FunctionAttributes {
                    is_public: def.is_public,
                    name: name.clone(),
                    params: def.params.to_owned(),
                    return_type: def.return_type.to_owned(),
                });
            } else {
                init_function = Some(FunctionAttributes {
                    is_public: def.is_public,
                    name: name.clone(),
                    params: def.params.to_owned(),
                    return_type: FixedSize::empty_tuple(),
                })
            }
        }

        let external_contracts = scope.borrow().get_module_type_defs(|typ| {
            if let Type::Contract(contract) = typ {
                // Skip our own contract
                if contract.name == scope.borrow().name {
                    None
                } else {
                    Some(contract.to_owned())
                }
            } else {
                None
            }
        });

        let structs = scope.borrow().get_module_type_defs(|typ| {
            if let Type::Struct(val) = typ {
                Some(val.to_owned())
            } else {
                None
            }
        });

        ContractAttributes {
            public_functions,
            init_function,
            events: scope
                .borrow()
                .event_defs
                .values()
                .map(|event| event.to_owned())
                .collect::<Vec<EventDef>>(),
            string_literals: scope.borrow().string_defs.clone(),
            structs,
            external_contracts,
            created_contracts: scope.borrow().created_contracts.to_owned(),
        }
    }
}

/// Contains contextual information relating to an expression AST node.
#[derive(Clone, Debug, PartialEq)]
pub struct ExpressionAttributes {
    pub typ: Type,
    pub location: Location,
    pub move_location: Option<Location>,
}

impl ExpressionAttributes {
    pub fn new(typ: Type, location: Location) -> Self {
        Self {
            typ,
            location,
            move_location: None,
        }
    }

    /// Return `true` if the type of the expression is an empty Tuple, otherwise
    /// `false`
    pub fn is_empty_tuple(&self) -> bool {
        if let ExpressionAttributes {
            typ: Type::Tuple(tuple),
            ..
        } = self
        {
            tuple.is_empty()
        } else {
            false
        }
    }

    /// Adds a move to memory, if it is already in memory.
    pub fn into_cloned(mut self) -> Result<Self, SemanticError> {
        if self.location != Location::Memory {
            Err(SemanticError::cannot_move())
        } else {
            self.move_location = Some(Location::Memory);
            Ok(self)
        }
    }

    /// Adds a move to memory, if it is in storage.
    pub fn into_cloned_from_sto(mut self) -> Result<Self, SemanticError> {
        if !matches!(self.location, Location::Storage { .. }) {
            Err(SemanticError::cannot_move())
        } else {
            self.move_location = Some(Location::Memory);
            Ok(self)
        }
    }

    /// Adds a move to value, if it is in storage or memory.
    pub fn into_loaded(mut self) -> Result<Self, SemanticError> {
        match self.typ {
            Type::Base(_) => {}
            Type::Contract(_) => {}
            _ => return Err(SemanticError::cannot_move()),
        }

        if self.location != Location::Value {
            self.move_location = Some(Location::Value);
        }

        Ok(self)
    }

    /// Adds a move (if necessary) to value if it is a base type and ensures
    /// reference types are in memory.
    pub fn into_assignable(self) -> Result<Self, SemanticError> {
        let assign_location = Location::assign_location(self.typ.to_owned())?;

        match assign_location {
            Location::Value => self.into_loaded(),
            Location::Memory => {
                if self.final_location() == Location::Memory {
                    Ok(self)
                } else {
                    Err(SemanticError::cannot_move())
                }
            }
            Location::Storage { .. } => unreachable!(),
        }
    }

    /// The final location of an expression after a possible move.
    pub fn final_location(&self) -> Location {
        if let Some(location) = self.move_location.clone() {
            return location;
        }

        self.location.clone()
    }
}

/// The type of a function call.
#[derive(Clone, Debug, PartialEq)]
pub enum CallType {
    BuiltinFunction { func: GlobalMethod },
    TypeConstructor { typ: Type },
    SelfAttribute { func_name: String },
    ValueAttribute,
    TypeAttribute { typ: Type, func_name: String },
}

/// Contains contextual information relating to a function definition AST node.
#[derive(Clone, Debug, PartialEq, PartialOrd, Eq, Ord)]
pub struct FunctionAttributes {
    pub is_public: bool,
    pub name: String,
    pub params: Vec<(String, FixedSize)>,
    pub return_type: FixedSize,
}

impl FunctionAttributes {
    pub fn param_types(&self) -> Vec<FixedSize> {
        self.params.iter().map(|(_, typ)| typ.to_owned()).collect()
    }

    pub fn param_names(&self) -> Vec<String> {
        self.params
            .iter()
            .map(|(name, _)| name.to_owned())
            .collect()
    }
}

impl From<ContractFunctionDef> for FunctionAttributes {
    fn from(def: ContractFunctionDef) -> Self {
        Self {
            is_public: def.is_public,
            name: def.name,
            params: def.params,
            return_type: def.return_type,
        }
    }
}

/// Contains contextual information about a Fe module and can be queried using
/// `Spanned` AST nodes.
#[derive(Clone, Debug, Default)]
pub struct Context {
    expressions: HashMap<NodeId, ExpressionAttributes>,
    emits: HashMap<NodeId, EventDef>,
    functions: HashMap<NodeId, FunctionAttributes>,
    declarations: HashMap<NodeId, FixedSize>,
    contracts: HashMap<NodeId, ContractAttributes>,
    calls: HashMap<NodeId, CallType>,
    events: HashMap<NodeId, EventDef>,
}

impl Context {
    pub fn new_shared() -> Shared<Self> {
        Rc::new(RefCell::new(Self::new()))
    }

    pub fn new() -> Self {
        Context {
            expressions: HashMap::new(),
            emits: HashMap::new(),
            functions: HashMap::new(),
            declarations: HashMap::new(),
            contracts: HashMap::new(),
            calls: HashMap::new(),
            events: HashMap::new(),
        }
    }

    /// Attribute contextual information to an expression node.
    pub fn add_expression<T: Into<NodeId>>(
        &mut self,
        node_id: T,
        attributes: ExpressionAttributes,
    ) {
        self.expressions.insert(node_id.into(), attributes);
    }

    /// Get information that has been attributed to an expression node.
    pub fn get_expression<T: Into<NodeId>>(&self, node_id: T) -> Option<&ExpressionAttributes> {
        self.expressions.get(&node_id.into())
    }

    /// Attribute contextual information to an emit statement node.
    pub fn add_emit<T: Into<NodeId>>(&mut self, node_id: T, event: EventDef) {
        self.emits.insert(node_id.into(), event);
    }

    /// Get information that has been attributed to an emit statement node.
    pub fn get_emit<T: Into<NodeId>>(&self, node_id: T) -> Option<&EventDef> {
        self.emits.get(&node_id.into())
    }

    /// Attribute contextual information to a function definition node.
    pub fn add_function<T: Into<NodeId>>(&mut self, node_id: T, attributes: FunctionAttributes) {
        self.functions.insert(node_id.into(), attributes);
    }

    /// Get information that has been attributed to a function definition node.
    pub fn get_function<T: Into<NodeId>>(&self, node_id: T) -> Option<&FunctionAttributes> {
        self.functions.get(&node_id.into())
    }

    /// Attribute contextual information to a declaration node.
    pub fn add_declaration<T: Into<NodeId>>(&mut self, node_id: T, typ: FixedSize) {
        self.declarations.insert(node_id.into(), typ);
    }

    /// Get information that has been attributed to a declaration node.
    pub fn get_declaration<T: Into<NodeId>>(&self, node_id: T) -> Option<&FixedSize> {
        self.declarations.get(&node_id.into())
    }

    /// Attribute contextual information to a contract definition node.
    pub fn add_contract<T: Into<NodeId>>(&mut self, node_id: T, attributes: ContractAttributes) {
        self.contracts.insert(node_id.into(), attributes);
    }

    /// Get information that has been attributed to a contract definition node.
    pub fn get_contract<T: Into<NodeId>>(&self, node_id: T) -> Option<&ContractAttributes> {
        self.contracts.get(&node_id.into())
    }

    /// Attribute contextual information to a call expression node.
    pub fn add_call<T: Into<NodeId>>(&mut self, node_id: T, call_type: CallType) {
        self.calls.insert(node_id.into(), call_type);
    }

    /// Get information that has been attributed to a call expression node.
    pub fn get_call<T: Into<NodeId>>(&self, node_id: T) -> Option<&CallType> {
        self.calls.get(&node_id.into())
    }

    /// Attribute contextual information to an event definition node.
    pub fn add_event<T: Into<NodeId>>(&mut self, node_id: T, event: EventDef) {
        self.events.insert(node_id.into(), event);
    }

    /// Get information that has been attributed to an event definition node.
    pub fn get_event<T: Into<NodeId>>(&self, node_id: T) -> Option<&EventDef> {
        self.events.get(&node_id.into())
    }
}

/// Performs semantic analysis of the source program and returns a `Context`
/// instance.
pub fn analyze(module: &fe::Module) -> Result<Context, SemanticError> {
    let context = Context::new_shared();
    traversal::module::module(Rc::clone(&context), module)?;
    Ok(Rc::try_unwrap(context)
        .map_err(|_| "more than one strong reference pointing to context")
        // This should never panic.
        .expect("failed to unwrap reference counter")
        .into_inner())
}

#[cfg(feature = "fix-context-harness")]
pub mod test_utils {
    use crate::namespace::types::FixedSize;
    use crate::{
        Context,
        ExpressionAttributes,
    };
    use fe_parser::ast as fe;
    use fe_parser::node::{
        Node,
        Span,
    };

    pub struct ContextHarness {
        pub context: Context,
        pub src: String,
    }

    impl ContextHarness {
        pub fn new(src: &str) -> Self {
            ContextHarness {
                context: Context::new(),
                src: src.to_string(),
            }
        }

        fn find_span(&self, substr: &str) -> Span {
            let start = self
                .src
                .find(substr)
                .unwrap_or_else(|| panic!("unable to find '{}' in '{}'", substr, self.src));

            Span {
                start,
                end: start + substr.len(),
            }
        }

        pub fn add_expression(&mut self, substr: &str, attributes: ExpressionAttributes) {
            let span = self.find_span(substr);
            let mock_node = Node::new(fe::Expr::Name("foo"), span);
            self.context.add_expression(&mock_node, attributes)
        }

        pub fn add_expressions(&mut self, substrs: Vec<&str>, attributes: ExpressionAttributes) {
            for substr in substrs {
                self.add_expression(substr, attributes.clone())
            }
        }

        pub fn add_declaration(&mut self, substr: &str, typ: FixedSize) {
            let span = self.find_span(substr);
            let mock_node = Node::new(
                fe::FuncStmt::Expr {
                    value: fe::Expr::Name("foo"),
                },
                span,
            );
            self.context.add_declaration(&mock_node, typ)
        }
    }
}
