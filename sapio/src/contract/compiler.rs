// Copyright Judica, Inc 2021
//
// This Source Code Form is subject to the terms of the Mozilla Public
//  License, v. 2.0. If a copy of the MPL was not distributed with this
//  file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The primary compilation traits and types
use super::actions::Guard;
use super::actions::{ConditionalCompileType, ConditionallyCompileIf};
use super::AnyContract;
use super::CompilationError;
use super::Compiled;
use super::Context;
use crate::contract::abi::continuation::ContinuationPoint;
use crate::util::amountrange::AmountRange;
use ::miniscript::*;
use sapio_base::Clause;
use std::collections::HashMap;
use std::collections::LinkedList;

enum CacheEntry<T> {
    Cached(Clause),
    Fresh(fn(&T, Context) -> Clause),
}

/// GuardCache assists with caching the computation of guard functions
/// during compilation.
struct GuardCache<T> {
    cache: HashMap<usize, Option<CacheEntry<T>>>,
}
impl<T> GuardCache<T> {
    fn new() -> Self {
        GuardCache {
            cache: HashMap::new(),
        }
    }
    fn create_entry(g: Option<Guard<T>>, t: &T, ctx: Context) -> Option<CacheEntry<T>> {
        Some(match g? {
            Guard::Cache(f) => CacheEntry::Cached(f(t, ctx)),
            Guard::Fresh(f) => CacheEntry::Fresh(f),
        })
    }
    fn get(&mut self, t: &T, f: fn() -> Option<Guard<T>>, ctx: Context) -> Option<Clause> {
        Some(
            match self
                .cache
                .entry(f as usize)
                .or_insert_with(|| Self::create_entry(f(), t, ctx.internal_clone()))
                .as_ref()?
            {
                CacheEntry::Cached(s) => s.clone(),
                CacheEntry::Fresh(f) => f(t, ctx),
            },
        )
    }
}

/// private::ImplSeal prevents anyone from implementing Compilable except by implementing Contract.
mod private {
    pub trait ImplSeal {}

    /// Allow Contract to implement Compile
    impl ImplSeal for super::Compiled {}
    impl<'a, C> ImplSeal for C where C: super::AnyContract {}
}
/// Compilable is a trait for anything which can be compiled
pub trait Compilable: private::ImplSeal {
    /// Compile a compilable object returning errors, if any.
    fn compile(&self, ctx: Context) -> Result<Compiled, CompilationError>;
}

/// Implements a basic identity
impl Compilable for Compiled {
    fn compile(&self, _ctx: Context) -> Result<Compiled, CompilationError> {
        Ok(self.clone())
    }
}

fn create_guards<T>(
    self_ref: &T,
    ctx: Context,
    guards: &[fn() -> Option<Guard<T>>],
    gc: &mut GuardCache<T>,
) -> Clause {
    guards
        .iter()
        .enumerate()
        .filter_map(|(i, x)| gc.get(self_ref, *x, ctx.derive(Some(&format!("{}", i)))))
        .filter(|x| *x != Clause::Trivial) // no point in using any Trivials
        .fold(Clause::Trivial, |acc, item| match acc {
            Clause::Trivial => item,
            _ => Clause::And(vec![acc, item]),
        })
}
impl<'a, T> Compilable for T
where
    T: AnyContract + 'a,
    T::Ref: 'a,
{
    /// The main Compilation Logic for a Contract.
    /// TODO: Better Document Semantics
    fn compile(&self, ctx: Context) -> Result<Compiled, CompilationError> {
        #[derive(PartialEq, Eq)]
        enum CTVRequired {
            Yes,
            No,
        }
        #[derive(PartialEq, Eq)]
        enum Nullable {
            Yes,
            No,
        }
        let self_ref = self.get_inner_ref();

        let guard_clauses = std::cell::RefCell::new(GuardCache::new());

        // The code for then_fns and finish_or_fns is very similar, differing
        // only in that then_fns have a CTV enforcing the contract and
        // finish_or_fns do not. We can lazily chain iterators to process them
        // in a row.
        let then_fns: Vec<_> = {
            let then_fn_ctx = ctx.derive(Some("then_fn"));
            let conditional_compile_ctx = then_fn_ctx.derive(Some("conditional_compile_if"));
            let guards_ctx = then_fn_ctx.derive(Some("guards"));
            let next_tx_ctx = then_fn_ctx.derive(Some("next_tx"));
            self.then_fns()
                .iter()
                .filter_map(|x| x())
                .enumerate()
                .filter_map(|(i, x)| {
                    let s = format!("{}", i);
                    let mut v = ConditionalCompileType::NoConstraint;
                    let this_ctx = conditional_compile_ctx.derive(Some(&s));
                    for (j, cond) in x
                        .conditional_compile_if
                        .iter()
                        .filter_map(|x| x())
                        .enumerate()
                    {
                        let ConditionallyCompileIf::Fresh(f) = cond;
                        v = v.merge(f(self_ref, this_ctx.derive(Some(&format!("{}", j)))));
                    }
                    match v {
                        ConditionalCompileType::Fail(v) => Some((s, v, Nullable::No, x)),
                        ConditionalCompileType::Required | ConditionalCompileType::NoConstraint => {
                            Some((s, LinkedList::new(), Nullable::No, x))
                        }
                        ConditionalCompileType::Skippable => None,
                        ConditionalCompileType::Never => None,
                        ConditionalCompileType::Nullable => {
                            Some((s, LinkedList::new(), Nullable::Yes, x))
                        }
                    }
                })
                .map(|(s, errors, nullability, x)| {
                    let guards = create_guards(
                        self_ref,
                        guards_ctx.derive(Some(&s)),
                        x.guard,
                        &mut guard_clauses.borrow_mut(),
                    );
                    if errors.is_empty() {
                        (
                            nullability,
                            CTVRequired::Yes,
                            guards,
                            (x.func)(self_ref, next_tx_ctx.derive(Some(&s))),
                        )
                    } else {
                        (
                            nullability,
                            CTVRequired::Yes,
                            guards,
                            Err(CompilationError::ConditionalCompilationFailed(errors)),
                        )
                    }
                })
                .collect()
        };
        // finish_or_fns may be used to compute additional transactions with
        // a given argument, but for building the ABI we only precompute with
        // the default argument.
        let finish_or_fns: Vec<_> = {
            let finish_or_fns_ctx = ctx.derive(Some("finish_or_fn"));
            let conditional_compile_ctx = finish_or_fns_ctx.derive(Some("conditional_compile_if"));
            let guard_ctx = finish_or_fns_ctx.derive(Some("guards"));
            let suggested_tx_ctx = finish_or_fns_ctx.derive(Some("suggested_txs"));
            self.finish_or_fns()
                .iter()
                .filter_map(|x| x())
                // TODO: De-duplicate this code?
                .enumerate()
                .filter_map(|(i, x)| {
                    let mut v = ConditionalCompileType::NoConstraint;
                    let s = format!("{}", i);
                    /// TODO: name?
                    let this_ctx = conditional_compile_ctx.derive(Some(&s));
                    for (i, cond) in x
                        .get_conditional_compile_if()
                        .iter()
                        .filter_map(|x| x())
                        .enumerate()
                    {
                        let ConditionallyCompileIf::Fresh(f) = cond;
                        v = v.merge(f(self_ref, this_ctx.derive(Some(&format!("{}", i)))));
                    }
                    match v {
                        ConditionalCompileType::Fail(v) => Some((s, v, x)),
                        ConditionalCompileType::Required | ConditionalCompileType::NoConstraint => {
                            Some((s, LinkedList::new(), x))
                        }
                        ConditionalCompileType::Skippable => None,
                        ConditionalCompileType::Never => None,
                        ConditionalCompileType::Nullable => Some((s, LinkedList::new(), x)),
                    }
                })
                .map(|(s, errors, x)| {
                    let guard = create_guards(
                        self_ref,
                        guard_ctx.derive(Some(&s)),
                        x.get_guard(),
                        &mut guard_clauses.borrow_mut(),
                    );
                    if errors.is_empty() {
                        let arg: T::StatefulArguments = Default::default();
                        let res = x.call(
                            self_ref,
                            suggested_tx_ctx.derive(Some(&s)),
                            arg,
                        );
                        (Nullable::Yes, CTVRequired::No, guard, res)
                    } else {
                        (
                            Nullable::Yes,
                            CTVRequired::No,
                            guard,
                            Err(CompilationError::ConditionalCompilationFailed(errors)),
                        )
                    }
                })
                .collect()
        };

        let continue_apis = self
            .finish_or_fns()
            .iter()
            .filter_map(|x| x())
            .filter_map(|f| {
                f.get_schema()
                    .clone()
                    .map(|s| (f.get_name().into(), ContinuationPoint { schema: s }))
            })
            .collect::<HashMap<String, ContinuationPoint>>();
        let mut ctv_to_tx = HashMap::new();
        let mut suggested_txs = HashMap::new();
        let mut amount_range = AmountRange::new();

        // If no guards and not CTV, then nothing gets added (not interpreted as Trivial True)
        // If CTV and no guards, just CTV added.
        // If CTV and guards, CTV & guards added.
        let mut clause_accumulator = then_fns
            .into_iter()
            .chain(finish_or_fns.into_iter())
            .map(|(nullability, uses_ctv, guards, r_txtmpls)| {
                // Compute all guard clauses.
                // Don't use a threshold here because then miniscript will just
                // re-compile it into the And for again, causing extra allocations.
                let mut guard = guards;

                // it would be an error if any of r_txtmpls is an error instead of just an empty
                // iterator.
                let mut txtmpl_clauses = r_txtmpls?
                    .map(|r_txtmpl| {
                        let txtmpl = r_txtmpl?;
                        let h = txtmpl.hash();
                        let txtmpl = match uses_ctv {
                            CTVRequired::Yes => &mut ctv_to_tx,
                            CTVRequired::No => &mut suggested_txs,
                        }
                        .entry(h)
                        .or_insert(txtmpl);
                        amount_range.update_range(txtmpl.max);
                        ctx.ctv_emulator(h)
                    })
                    // Forces any error to abort the whole thing
                    .collect::<Result<Vec<_>, CompilationError>>()?;
                if uses_ctv == CTVRequired::Yes {
                    if nullability == Nullable::Yes && txtmpl_clauses.is_empty() {
                        // Mark this branch dead.
                        guard = Clause::Unsatisfiable;
                    } else {
                        let hashes = match txtmpl_clauses.len() {
                            0 => {
                                return Err(CompilationError::MissingTemplates);
                            }
                            1 => txtmpl_clauses
                                .pop()
                                .expect("Length of txtmpl_clauses must be at least 1"),
                            _n => Clause::Threshold(1, txtmpl_clauses),
                        };
                        guard = match guard {
                            Clause::Trivial => hashes,
                            _ => Clause::And(vec![guard, hashes]),
                        };
                    }
                }
                Ok(guard)
            })
            .filter_map(|x| {
                if let Ok(Clause::Unsatisfiable) = x {
                    None
                } else {
                    Some(x)
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let finish_fns: Vec<_> = {
            let finish_fns_ctx = ctx.derive(Some("finish_fn"));
            // Compute all finish_functions at this level, caching if requested.
            self.finish_fns()
                .iter()
                .enumerate()
                .filter_map(|(i, x)| {
                    guard_clauses.borrow_mut().get(
                        self_ref,
                        *x,
                        finish_fns_ctx.derive(Some(&format!("{}", i))),
                    )
                })
                .collect()
        };
        // If any clauses are returned, use a Threshold with n = 1
        // It compiles equivalently to a tree of ORs.
        if finish_fns.len() > 0 {
            clause_accumulator.push(Clause::Threshold(1, finish_fns))
        }

        let policy = match clause_accumulator.len() {
            0 => return Err(CompilationError::EmptyPolicy),
            1 => clause_accumulator
                .pop()
                .expect("Length of policy must be at least 1"),
            _ => Clause::Threshold(1, clause_accumulator),
        };

        let miniscript = policy.compile().map_err(Into::<CompilationError>::into)?;
        let descriptor = Descriptor::new_wsh(miniscript)?;
        let address = descriptor.address(ctx.network)?.into();
        let descriptor = Some(descriptor);
        let policy = Some(policy);

        Ok(Compiled {
            ctv_to_tx,
            suggested_txs,
            continue_apis,
            address,
            descriptor,
            policy,
            amount_range,
        })
    }
}
