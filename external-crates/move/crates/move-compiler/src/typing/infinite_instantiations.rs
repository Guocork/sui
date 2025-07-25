// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

use super::core::{self, Subst, TParamSubst};
use crate::{
    diagnostics::{Diagnostic, codes::TypeSafety},
    expansion::ast::ModuleIdent,
    naming::ast::{self as N, TParam, TParamID, Type, Type_},
    parser::ast::FunctionName,
    shared::{CompilationEnv, unique_map::UniqueMap},
    typing::ast as T,
};
use move_ir_types::location::*;
use move_proc_macros::growing_stack;
use move_symbol_pool::Symbol;
use petgraph::{
    algo::{astar as petgraph_astar, tarjan_scc as petgraph_scc},
    graphmap::DiGraphMap,
};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edge {
    Identity,
    Nested,
}

#[derive(Clone, Debug)]
struct EdgeInfo {
    name: FunctionName,
    type_argument: Type,
    loc: Loc,
    edge: Edge,
}

struct Context<'a> {
    tparams: &'a BTreeMap<ModuleIdent, BTreeMap<FunctionName, &'a Vec<TParam>>>,
    tparam_type_arguments: BTreeMap<TParam, BTreeMap<TParam, EdgeInfo>>,
    current_module: ModuleIdent,
}

impl<'a> Context<'a> {
    fn new(
        tparams: &'a BTreeMap<ModuleIdent, BTreeMap<FunctionName, &'a Vec<TParam>>>,
        current_module: ModuleIdent,
    ) -> Self {
        Context {
            tparams,
            current_module,
            tparam_type_arguments: BTreeMap::new(),
        }
    }

    fn add_usage(&mut self, loc: Loc, module: &ModuleIdent, fname: &FunctionName, targs: &[Type]) {
        if &self.current_module != module {
            return;
        }
        self.tparams[module][fname]
            .iter()
            .zip(targs)
            .for_each(|(tparam, targ)| {
                let info = EdgeInfo {
                    name: *fname,
                    type_argument: targ.clone(),
                    loc,
                    edge: Edge::Identity,
                };
                Self::add_tparam_edges(&mut self.tparam_type_arguments, tparam, info, targ)
            })
    }

    fn add_tparam_edges(
        acc: &mut BTreeMap<TParam, BTreeMap<TParam, EdgeInfo>>,
        tparam: &TParam,
        info: EdgeInfo,
        sp!(_, targ_): &Type,
    ) {
        use N::Type_::*;
        match targ_ {
            Var(_) => panic!("ICE tvar after expansion"),
            Unit | Anything | Void | UnresolvedError => (),
            Ref(_, t) => {
                let info = EdgeInfo {
                    edge: Edge::Nested,
                    ..info
                };
                Self::add_tparam_edges(acc, tparam, info, t)
            }
            Apply(_, _, tys) => {
                let info = EdgeInfo {
                    edge: Edge::Nested,
                    ..info
                };
                tys.iter()
                    .for_each(|t| Self::add_tparam_edges(acc, tparam, info.clone(), t))
            }
            Fun(tys, t) => {
                let info = EdgeInfo {
                    edge: Edge::Nested,
                    ..info
                };
                tys.iter()
                    .for_each(|t| Self::add_tparam_edges(acc, tparam, info.clone(), t));
                Self::add_tparam_edges(acc, tparam, info.clone(), t)
            }
            Param(tp) => {
                let tp_neighbors = acc.entry(tp.clone()).or_default();
                match tp_neighbors.get(tparam) {
                    Some(EdgeInfo {
                        edge: Edge::Nested, ..
                    }) => (),
                    None
                    | Some(EdgeInfo {
                        edge: Edge::Identity,
                        ..
                    }) => {
                        tp_neighbors.insert(tparam.clone(), info);
                    }
                }
            }
        }
    }

    fn instantiation_graph(&self) -> DiGraphMap<&TParam, Edge> {
        let edges = self
            .tparam_type_arguments
            .iter()
            .flat_map(|(parent, children)| {
                children
                    .iter()
                    .map(move |(child, info)| (parent, child, info.edge))
            });
        DiGraphMap::from_edges(edges)
    }
}

//**************************************************************************************************
// Modules
//**************************************************************************************************

pub fn modules(
    compilation_env: &CompilationEnv,
    modules: &UniqueMap<ModuleIdent, T::ModuleDefinition>,
) {
    let tparams = modules
        .key_cloned_iter()
        .map(|(mname, mdef)| {
            let tparams = mdef
                .functions
                .key_cloned_iter()
                .map(|(fname, fdef)| (fname, &fdef.signature.type_parameters))
                .collect();
            (mname, tparams)
        })
        .collect();
    modules
        .key_cloned_iter()
        .for_each(|(mname, m)| module(compilation_env, &tparams, mname, m))
}

macro_rules! scc_edges {
    ($graph:expr, $scc:expr) => {{
        let g = $graph;
        let s = $scc;
        s.iter().flat_map(move |v| {
            s.iter()
                .filter_map(move |u| g.edge_weight(v, u).cloned().map(|e| (v, e, u)))
        })
    }};
}

fn module<'a>(
    compilation_env: &CompilationEnv,
    tparams: &'a BTreeMap<ModuleIdent, BTreeMap<FunctionName, &'a Vec<TParam>>>,
    mname: ModuleIdent,
    module: &T::ModuleDefinition,
) {
    let reporter = compilation_env.diagnostic_reporter_at_top_level();
    let context = &mut Context::new(tparams, mname);
    module
        .functions
        .key_cloned_iter()
        .for_each(|(_fname, fdef)| function_body(context, &fdef.body));
    let graph = context.instantiation_graph();
    // - get the strongly connected components
    // - fitler out SCCs that do not contain a 'nested' or 'strong' edge
    // - report those cycles
    petgraph_scc(&graph)
        .into_iter()
        .filter(|scc| scc_edges!(&graph, scc).any(|(_, e, _)| e == Edge::Nested))
        .for_each(|scc| reporter.add_diag(cycle_error(context, &graph, scc)))
}

//**************************************************************************************************
// Functions
//**************************************************************************************************

fn function_body(context: &mut Context, sp!(_, b_): &T::FunctionBody) {
    match b_ {
        T::FunctionBody_::Native | T::FunctionBody_::Macro => (),
        T::FunctionBody_::Defined(es) => sequence(context, es),
    }
}

//**************************************************************************************************
// Expressions
//**************************************************************************************************

fn sequence(context: &mut Context, (_, seq): &T::Sequence) {
    seq.iter().for_each(|item| sequence_item(context, item))
}

fn sequence_item(context: &mut Context, item: &T::SequenceItem) {
    use T::SequenceItem_ as S;
    match &item.value {
        S::Bind(_, _, te) | S::Seq(te) => exp(context, te),
        S::Declare(_) => (),
    }
}

#[growing_stack]
fn exp(context: &mut Context, e: &T::Exp) {
    use T::UnannotatedExp_ as E;
    match &e.exp.value {
        E::Use(_) => panic!("ICE should have been expanded"),

        E::Unit { .. }
        | E::Value(_)
        | E::Constant(_, _)
        | E::Move { .. }
        | E::Copy { .. }
        | E::BorrowLocal(_, _)
        | E::Continue(_)
        | E::ErrorConstant { .. }
        | E::UnresolvedError => (),

        E::ModuleCall(call) => {
            context.add_usage(e.exp.loc, &call.module, &call.name, &call.type_arguments);
            exp(context, &call.arguments)
        }

        E::IfElse(eb, et, ef_opt) => {
            exp(context, eb);
            exp(context, et);
            if let Some(ef) = ef_opt {
                exp(context, ef)
            }
        }
        E::Match(esubject, arms) => {
            exp(context, esubject);
            for sp!(_, arm) in &arms.value {
                if let Some(guard) = arm.guard.as_ref() {
                    exp(context, guard)
                }
                exp(context, &arm.rhs);
            }
        }
        E::VariantMatch(subject, _, arms) => {
            exp(context, subject);
            for (_, rhs) in arms {
                exp(context, rhs);
            }
        }
        E::While(_, eb, eloop) => {
            exp(context, eb);
            exp(context, eloop);
        }
        E::Loop { body: eloop, .. } => exp(context, eloop),
        E::NamedBlock(_, seq) => sequence(context, seq),
        E::Block(seq) => sequence(context, seq),
        E::Assign(_, _, er) => exp(context, er),

        E::Builtin(_, base_exp)
        | E::Vector(_, _, _, base_exp)
        | E::Return(base_exp)
        | E::Abort(base_exp)
        | E::Give(_, base_exp)
        | E::Dereference(base_exp)
        | E::UnaryExp(_, base_exp)
        | E::Borrow(_, base_exp, _)
        | E::TempBorrow(_, base_exp) => exp(context, base_exp),
        E::Mutate(el, er) | E::BinopExp(el, _, _, er) => {
            exp(context, el);
            exp(context, er)
        }

        E::Pack(_, _, _, fields) => {
            for (_, _, (_, (_, fe))) in fields.iter() {
                exp(context, fe)
            }
        }
        E::PackVariant(_, _, _, _, fields) => {
            for (_, _, (_, (_, fe))) in fields.iter() {
                exp(context, fe)
            }
        }

        E::ExpList(el) => exp_list(context, el),

        E::Cast(e, _) | E::Annotate(e, _) => exp(context, e),
    }
}

fn exp_list(context: &mut Context, items: &[T::ExpListItem]) {
    items.iter().for_each(|item| exp_list_item(context, item))
}

fn exp_list_item(context: &mut Context, item: &T::ExpListItem) {
    use T::ExpListItem as I;
    match item {
        I::Single(e, _) | I::Splat(_, e, _) => {
            exp(context, e);
        }
    }
}

//**************************************************************************************************
// Errors
//**************************************************************************************************

fn cycle_error(
    context: &Context,
    graph: &DiGraphMap<&TParam, Edge>,
    scc: Vec<&TParam>,
) -> Diagnostic {
    let critical_edge = scc_edges!(graph, &scc).find(|(_, e, _)| e == &Edge::Nested);
    // tail -> head
    let (critical_tail, _, critical_head) = critical_edge.unwrap();
    let (_, cycle_nodes) = petgraph_astar(
        graph,
        critical_head,
        |finish| &finish == critical_tail,
        |_e| 1,
        |_| 0,
    )
    .unwrap();
    assert!(!cycle_nodes.is_empty());
    let next = |i| (i + 1) % cycle_nodes.len();
    let prev = |i: usize| i.checked_sub(1).unwrap_or(cycle_nodes.len() - 1);

    assert!(&cycle_nodes[0] == critical_head);
    let param_info = &context.tparam_type_arguments[cycle_nodes[0]][cycle_nodes[next(0)]];
    let arg_info = &context.tparam_type_arguments[cycle_nodes[prev(0)]][cycle_nodes[0]];

    let call_loc = arg_info.loc;
    let call_msg = format!(
        "Invalid call to '{}::{}'",
        &context.current_module, &arg_info.name,
    );
    let ty_loc = arg_info.type_argument.loc;
    let ty_str = core::error_format(&arg_info.type_argument, &Subst::empty());
    let case = match cycle_nodes.len() {
        1 => "This recursive call",
        2 => "These mutually recursive calls",
        _ => "A cycle of recursive calls",
    };
    let tparam_msg = format!(
        "The type parameter '{param_n}::{param_t}' was instantiated with the type {ty}, which \
         contains the type parameter '{arg_n}::{arg_t}'. {case} causes the instantiation to \
         recurse infinitely",
        param_n = &param_info.name,
        param_t = &critical_head.user_specified_name,
        ty = ty_str,
        arg_n = &arg_info.name,
        arg_t = &critical_tail.user_specified_name,
        case = case,
    );

    let mut secondary_labels = vec![(ty_loc, tparam_msg)];

    if cycle_nodes.len() > 1 {
        let (mut subst, init_call) = {
            let ftparam = cycle_nodes[0];
            let prev_tparam = cycle_nodes[prev(0)];
            let init_state = &context.tparam_type_arguments[prev_tparam][ftparam];
            let ftparam_ty = {
                let qualified_ = Symbol::from(format!(
                    "{}::{}",
                    &init_state.name, &ftparam.user_specified_name
                ));
                let qualified = sp(ftparam.user_specified_name.loc, qualified_);
                let qualified_tp = TParam {
                    user_specified_name: qualified,
                    ..ftparam.clone()
                };
                sp(init_state.loc, Type_::Param(qualified_tp))
            };
            let init_call = make_call_string(context, init_state, ftparam.id, &ftparam_ty);
            let loc = ftparam.user_specified_name.loc;
            let subst = make_subst(context, loc, init_state, ftparam.id, ftparam_ty);
            (subst, init_call)
        };

        let cycle_calls = cycle_nodes
            .iter()
            .enumerate()
            .map(|(i, targ_tparam)| {
                let tparam = cycle_nodes[next(i)];
                let cur = &context.tparam_type_arguments[targ_tparam][tparam];
                let targ = core::subst_tparams(&subst, cur.type_argument.clone());
                let res = make_call_string(context, cur, tparam.id, &targ);
                let loc = tparam.user_specified_name.loc;
                subst = make_subst(context, loc, cur, tparam.id, targ);
                res
            })
            .collect::<Vec<_>>();
        cycle_calls
            .iter()
            .enumerate()
            .for_each(|(i, (loc, next_call))| {
                let (_, prev_call) = if i == 0 {
                    &init_call
                } else {
                    &cycle_calls[prev(i)]
                };
                let msg = format!("'{}' calls '{}'", prev_call, next_call);
                secondary_labels.push((*loc, msg))
            });
    }

    Diagnostic::new(
        TypeSafety::CyclicInstantiation,
        (call_loc, call_msg),
        secondary_labels,
        std::iter::empty::<String>(),
    )
}

fn make_subst(
    context: &Context,
    loc: Loc,
    state: &EdgeInfo,
    tparam: TParamID,
    tparam_ty: Type,
) -> TParamSubst {
    let mut tparam_ty = Some(tparam_ty);
    context.tparams[&context.current_module][&state.name]
        .iter()
        .map(|tp| {
            let ty = if tp.id == tparam {
                tparam_ty.take().unwrap()
            } else {
                sp(loc, Type_::Anything)
            };
            (tp.id, ty)
        })
        .collect::<TParamSubst>()
}

fn make_call_string(
    context: &Context,
    cur: &EdgeInfo,
    tparam: TParamID,
    targ: &Type,
) -> (Loc, String) {
    let targs = context.tparams[&context.current_module][&cur.name]
        .iter()
        .map(|tp| {
            if tp.id == tparam {
                core::error_format_nested(targ, &Subst::empty())
            } else {
                "_".to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let targs = if targs.is_empty() {
        targs
    } else {
        format!("<{}>", targs)
    };
    (cur.loc, format!("{}{}", &cur.name, targs))
}
