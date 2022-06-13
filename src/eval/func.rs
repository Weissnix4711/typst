use std::fmt::{self, Debug, Formatter};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use super::{Args, Eval, Flow, Machine, Scope, Scopes, Value};
use crate::diag::{StrResult, TypResult};
use crate::model::{Content, NodeId, StyleMap};
use crate::source::SourceId;
use crate::syntax::ast::Expr;
use crate::util::EcoString;
use crate::Context;

/// An evaluatable function.
#[derive(Clone, Hash)]
pub struct Func(Arc<Repr>);

/// The different kinds of function representations.
#[derive(Hash)]
enum Repr {
    /// A native rust function.
    Native(Native),
    /// A user-defined closure.
    Closure(Closure),
    /// A nested function with pre-applied arguments.
    With(Func, Args),
}

impl Func {
    /// Create a new function from a native rust function.
    pub fn from_fn(
        name: &'static str,
        func: fn(&mut Machine, &mut Args) -> TypResult<Value>,
    ) -> Self {
        Self(Arc::new(Repr::Native(Native {
            name,
            func,
            set: None,
            node: None,
        })))
    }

    /// Create a new function from a native rust node.
    pub fn from_node<T: Node>(name: &'static str) -> Self {
        Self(Arc::new(Repr::Native(Native {
            name,
            func: |ctx, args| {
                let styles = T::set(args, true)?;
                let content = T::construct(ctx, args)?;
                Ok(Value::Content(content.styled_with_map(styles.scoped())))
            },
            set: Some(|args| T::set(args, false)),
            node: T::SHOWABLE.then(|| NodeId::of::<T>()),
        })))
    }

    /// Create a new function from a closure.
    pub fn from_closure(closure: Closure) -> Self {
        Self(Arc::new(Repr::Closure(closure)))
    }

    /// Apply the given arguments to the function.
    pub fn with(self, args: Args) -> Self {
        Self(Arc::new(Repr::With(self, args)))
    }

    /// The name of the function.
    pub fn name(&self) -> Option<&str> {
        match self.0.as_ref() {
            Repr::Native(native) => Some(native.name),
            Repr::Closure(closure) => closure.name.as_deref(),
            Repr::With(func, _) => func.name(),
        }
    }

    /// The number of positional arguments this function takes, if known.
    pub fn argc(&self) -> Option<usize> {
        match self.0.as_ref() {
            Repr::Closure(closure) => Some(
                closure.params.iter().filter(|(_, default)| default.is_none()).count(),
            ),
            Repr::With(wrapped, applied) => Some(wrapped.argc()?.saturating_sub(
                applied.items.iter().filter(|arg| arg.name.is_none()).count(),
            )),
            _ => None,
        }
    }

    /// Call the function with the given arguments.
    pub fn call(&self, vm: &mut Machine, mut args: Args) -> TypResult<Value> {
        let value = match self.0.as_ref() {
            Repr::Native(native) => (native.func)(vm, &mut args)?,
            Repr::Closure(closure) => closure.call(vm, &mut args)?,
            Repr::With(wrapped, applied) => {
                args.items.splice(.. 0, applied.items.iter().cloned());
                return wrapped.call(vm, args);
            }
        };
        args.finish()?;
        Ok(value)
    }

    /// Call the function without an existing virtual machine.
    pub fn call_detached(&self, ctx: &mut Context, args: Args) -> TypResult<Value> {
        let mut vm = Machine::new(ctx, vec![], Scopes::new(None));
        self.call(&mut vm, args)
    }

    /// Execute the function's set rule and return the resulting style map.
    pub fn set(&self, mut args: Args) -> TypResult<StyleMap> {
        let styles = match self.0.as_ref() {
            Repr::Native(Native { set: Some(set), .. }) => set(&mut args)?,
            _ => StyleMap::new(),
        };
        args.finish()?;
        Ok(styles)
    }

    /// The id of the node to customize with this function's show rule.
    pub fn node(&self) -> StrResult<NodeId> {
        match self.0.as_ref() {
            Repr::Native(Native { node: Some(id), .. }) => Ok(*id),
            _ => Err("this function cannot be customized with show")?,
        }
    }
}

impl Debug for Func {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self.name() {
            Some(name) => f.write_str(name),
            None => f.write_str("(..) => {..}"),
        }
    }
}

impl PartialEq for Func {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// A function defined by a native rust function or node.
struct Native {
    /// The name of the function.
    pub name: &'static str,
    /// The function pointer.
    pub func: fn(&mut Machine, &mut Args) -> TypResult<Value>,
    /// The set rule.
    pub set: Option<fn(&mut Args) -> TypResult<StyleMap>>,
    /// The id of the node to customize with this function's show rule.
    pub node: Option<NodeId>,
}

impl Hash for Native {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        (self.func as usize).hash(state);
        self.set.map(|set| set as usize).hash(state);
        self.node.hash(state);
    }
}

/// A constructable, stylable content node.
pub trait Node: 'static {
    /// Whether this node can be customized through a show rule.
    const SHOWABLE: bool;

    /// Construct a node from the arguments.
    ///
    /// This is passed only the arguments that remain after execution of the
    /// node's set rule.
    fn construct(vm: &mut Machine, args: &mut Args) -> TypResult<Content>;

    /// Parse relevant arguments into style properties for this node.
    ///
    /// When `constructor` is true, [`construct`](Self::construct) will run
    /// after this invocation of `set` with the remaining arguments.
    fn set(args: &mut Args, constructor: bool) -> TypResult<StyleMap>;
}

/// A user-defined closure.
#[derive(Hash)]
pub struct Closure {
    /// The source file where the closure was defined.
    pub location: Option<SourceId>,
    /// The name of the closure.
    pub name: Option<EcoString>,
    /// Captured values from outer scopes.
    pub captured: Scope,
    /// The parameter names and default values. Parameters with default value
    /// are named parameters.
    pub params: Vec<(EcoString, Option<Value>)>,
    /// The name of an argument sink where remaining arguments are placed.
    pub sink: Option<EcoString>,
    /// The expression the closure should evaluate to.
    pub body: Expr,
}

impl Closure {
    /// Call the function in the context with the arguments.
    pub fn call(&self, vm: &mut Machine, args: &mut Args) -> TypResult<Value> {
        // Don't leak the scopes from the call site. Instead, we use the scope
        // of captured variables we collected earlier.
        let mut scopes = Scopes::new(None);
        scopes.top = self.captured.clone();

        // Parse the arguments according to the parameter list.
        for (param, default) in &self.params {
            scopes.top.define(param, match default {
                None => args.expect::<Value>(param)?,
                Some(default) => {
                    args.named::<Value>(param)?.unwrap_or_else(|| default.clone())
                }
            });
        }

        // Put the remaining arguments into the sink.
        if let Some(sink) = &self.sink {
            scopes.top.define(sink, args.take());
        }

        // Determine the route inside the closure.
        let detached = vm.route.is_empty();
        let route = if detached {
            self.location.into_iter().collect()
        } else {
            vm.route.clone()
        };

        // Evaluate the body.
        let mut sub = Machine::new(vm.ctx, route, scopes);
        let result = self.body.eval(&mut sub);
        vm.deps.extend(sub.deps);

        // Handle control flow.
        match sub.flow {
            Some(Flow::Return(_, Some(explicit))) => return Ok(explicit),
            Some(Flow::Return(_, None)) => {}
            Some(flow) => return Err(flow.forbidden())?,
            None => {}
        }

        result
    }
}
