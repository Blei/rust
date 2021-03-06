// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

/*

# Collect phase

The collect phase of type check has the job of visiting all items,
determining their type, and writing that type into the `tcx.tcache`
table.  Despite its name, this table does not really operate as a
*cache*, at least not for the types of items defined within the
current crate: we assume that after the collect phase, the types of
all local items will be present in the table.

Unlike most of the types that are present in Rust, the types computed
for each item are in fact polytypes.  In "layman's terms", this means
that they are generic types that may have type parameters (more
mathematically phrased, they are universally quantified over a set of
type parameters).  Polytypes are represented by an instance of
`ty::ty_param_bounds_and_ty`.  This combines the core type along with
a list of the bounds for each parameter.  Type parameters themselves
are represented as `ty_param()` instances.

*/


use metadata::csearch;
use middle::resolve_lifetime;
use middle::ty::{ImplContainer, MethodContainer, TraitContainer, substs};
use middle::ty::{ty_param_bounds_and_ty};
use middle::ty;
use middle::subst::Subst;
use middle::typeck::astconv::{AstConv, ty_of_arg};
use middle::typeck::astconv::{ast_ty_to_ty};
use middle::typeck::astconv;
use middle::typeck::rscope::*;
use middle::typeck::{CrateCtxt, lookup_def_tcx, no_params, write_ty_to_tcx};
use util::ppaux;
use util::ppaux::Repr;

use std::rc::Rc;
use collections::{HashMap, HashSet};

use syntax::abi;
use syntax::ast::{RegionTyParamBound, TraitTyParamBound};
use syntax::ast;
use syntax::ast_map;
use syntax::ast_util::{local_def, split_trait_methods};
use syntax::codemap::Span;
use syntax::codemap;
use syntax::parse::token::special_idents;
use syntax::parse::token;
use syntax::print::pprust::{path_to_str};
use syntax::visit;
use syntax::owned_slice::OwnedSlice;

struct CollectItemTypesVisitor<'a> {
    ccx: &'a CrateCtxt<'a>
}

impl<'a> visit::Visitor<()> for CollectItemTypesVisitor<'a> {
    fn visit_item(&mut self, i: &ast::Item, _: ()) {
        convert(self.ccx, i);
        visit::walk_item(self, i, ());
    }
    fn visit_foreign_item(&mut self, i: &ast::ForeignItem, _: ()) {
        convert_foreign(self.ccx, i);
        visit::walk_foreign_item(self, i, ());
    }
}

pub fn collect_item_types(ccx: &CrateCtxt, krate: &ast::Crate) {
    fn collect_intrinsic_type(ccx: &CrateCtxt,
                              lang_item: ast::DefId) {
        let ty::ty_param_bounds_and_ty { ty: ty, .. } =
            ccx.get_item_ty(lang_item);
        ccx.tcx.intrinsic_defs.borrow_mut().insert(lang_item, ty);
    }

    match ccx.tcx.lang_items.ty_desc() {
        Some(id) => { collect_intrinsic_type(ccx, id); } None => {}
    }
    match ccx.tcx.lang_items.opaque() {
        Some(id) => { collect_intrinsic_type(ccx, id); } None => {}
    }

    let mut visitor = CollectItemTypesVisitor{ ccx: ccx };
    visit::walk_crate(&mut visitor, krate, ());
}

pub trait ToTy {
    fn to_ty<RS:RegionScope>(&self, rs: &RS, ast_ty: &ast::Ty) -> ty::t;
}

impl<'a> ToTy for CrateCtxt<'a> {
    fn to_ty<RS:RegionScope>(&self, rs: &RS, ast_ty: &ast::Ty) -> ty::t {
        ast_ty_to_ty(self, rs, ast_ty)
    }
}

impl<'a> AstConv for CrateCtxt<'a> {
    fn tcx<'a>(&'a self) -> &'a ty::ctxt { self.tcx }

    fn get_item_ty(&self, id: ast::DefId) -> ty::ty_param_bounds_and_ty {
        if id.krate != ast::LOCAL_CRATE {
            return csearch::get_type(self.tcx, id)
        }

        match self.tcx.map.find(id.node) {
            Some(ast_map::NodeItem(item)) => ty_of_item(self, item),
            Some(ast_map::NodeForeignItem(foreign_item)) => {
                let abi = self.tcx.map.get_foreign_abi(id.node);
                ty_of_foreign_item(self, foreign_item, abi)
            }
            x => {
                self.tcx.sess.bug(format!("unexpected sort of node \
                                           in get_item_ty(): {:?}", x));
            }
        }
    }

    fn get_trait_def(&self, id: ast::DefId) -> @ty::TraitDef {
        get_trait_def(self, id)
    }

    fn ty_infer(&self, span: Span) -> ty::t {
        self.tcx.sess.span_err(span, "the type placeholder `_` is not \
                                      allowed within types on item signatures.");
        ty::mk_err()
    }
}

pub fn get_enum_variant_types(ccx: &CrateCtxt,
                              enum_ty: ty::t,
                              variants: &[ast::P<ast::Variant>],
                              generics: &ast::Generics) {
    let tcx = ccx.tcx;

    // Create a set of parameter types shared among all the variants.
    for variant in variants.iter() {
        // Nullary enum constructors get turned into constants; n-ary enum
        // constructors get turned into functions.
        let scope = variant.node.id;
        let result_ty = match variant.node.kind {
            ast::TupleVariantKind(ref args) if args.len() > 0 => {
                let rs = ExplicitRscope;
                let input_tys: Vec<_> = args.iter().map(|va| ccx.to_ty(&rs, va.ty)).collect();
                ty::mk_ctor_fn(tcx, scope, input_tys.as_slice(), enum_ty)
            }

            ast::TupleVariantKind(_) => {
                enum_ty
            }

            ast::StructVariantKind(struct_def) => {
                let tpt = ty_param_bounds_and_ty {
                    generics: ty_generics_for_type(ccx, generics),
                    ty: enum_ty
                };

                convert_struct(ccx, struct_def, tpt, variant.node.id);

                let input_tys: Vec<_> = struct_def.fields.iter().map(
                    |f| ty::node_id_to_type(ccx.tcx, f.node.id)).collect();
                ty::mk_ctor_fn(tcx, scope, input_tys.as_slice(), enum_ty)
            }
        };

        let tpt = ty_param_bounds_and_ty {
            generics: ty_generics_for_type(ccx, generics),
            ty: result_ty
        };

        tcx.tcache.borrow_mut().insert(local_def(variant.node.id), tpt);

        write_ty_to_tcx(tcx, variant.node.id, result_ty);
    }
}

pub fn ensure_trait_methods(ccx: &CrateCtxt, trait_id: ast::NodeId) {
    let tcx = ccx.tcx;
    match tcx.map.get(trait_id) {
        ast_map::NodeItem(item) => {
            match item.node {
                ast::ItemTrait(ref generics, _, ref ms) => {
                    let trait_ty_generics = ty_generics_for_type(ccx, generics);

                    // For each method, construct a suitable ty::Method and
                    // store it into the `tcx.methods` table:
                    for m in ms.iter() {
                        let ty_method = @match m {
                            &ast::Required(ref m) => {
                                ty_method_of_trait_method(
                                    ccx, trait_id, &trait_ty_generics,
                                    &m.id, &m.ident, &m.explicit_self,
                                    &m.generics, &m.fn_style, m.decl)
                            }

                            &ast::Provided(ref m) => {
                                ty_method_of_trait_method(
                                    ccx, trait_id, &trait_ty_generics,
                                    &m.id, &m.ident, &m.explicit_self,
                                    &m.generics, &m.fn_style, m.decl)
                            }
                        };

                        if ty_method.explicit_self == ast::SelfStatic {
                            make_static_method_ty(ccx, trait_id, ty_method,
                                                  &trait_ty_generics);
                        }

                        tcx.methods.borrow_mut().insert(ty_method.def_id,
                                                        ty_method);
                    }

                    // Add an entry mapping
                    let method_def_ids = @ms.iter().map(|m| {
                        match m {
                            &ast::Required(ref ty_method) => {
                                local_def(ty_method.id)
                            }
                            &ast::Provided(ref method) => {
                                local_def(method.id)
                            }
                        }
                    }).collect();

                    let trait_def_id = local_def(trait_id);
                    tcx.trait_method_def_ids.borrow_mut()
                        .insert(trait_def_id, method_def_ids);
                }
                _ => {} // Ignore things that aren't traits.
            }
        }
        _ => { /* Ignore things that aren't traits */ }
    }

    fn make_static_method_ty(ccx: &CrateCtxt,
                             trait_id: ast::NodeId,
                             m: &ty::Method,
                             trait_ty_generics: &ty::Generics) {
        // If declaration is
        //
        //     trait Trait<'a,'b,'c,a,b,c> {
        //        fn foo<'d,'e,'f,d,e,f>(...) -> Self;
        //     }
        //
        // and we will create a function like
        //
        //     fn foo<'a,'b,'c,   // First the lifetime params from trait
        //            'd,'e,'f,   // Then lifetime params from `foo()`
        //            a,b,c,      // Then type params from trait
        //            D:Trait<'a,'b,'c,a,b,c>, // Then this sucker
        //            E,F,G       // Then type params from `foo()`, offset by 1
        //           >(...) -> D' {}
        //
        // Note that `Self` is replaced with an explicit type
        // parameter D that is sandwiched in between the trait params
        // and the method params, and thus the indices of the method
        // type parameters are offset by 1 (that is, the method
        // parameters are mapped from d, e, f to E, F, and G).  The
        // choice of this ordering is somewhat arbitrary.
        //
        // Note also that the bound for `D` is `Trait<'a,'b,'c,a,b,c>`.
        // This implies that the lifetime parameters that were inherited
        // from the trait (i.e., `'a`, `'b`, and `'c`) all must be early
        // bound, since they appear in a trait bound.
        //
        // Also, this system is rather a hack that should be replaced
        // with a more uniform treatment of Self (which is partly
        // underway).

        // build up a subst that shifts all of the parameters over
        // by one and substitute in a new type param for self

        let tcx = ccx.tcx;

        let dummy_defid = ast::DefId {krate: 0, node: 0};

        // Represents [A',B',C']
        let num_trait_bounds = trait_ty_generics.type_param_defs().len();
        let non_shifted_trait_tps = Vec::from_fn(num_trait_bounds, |i| {
            ty::mk_param(tcx, i, trait_ty_generics.type_param_defs()[i].def_id)
        });

        // Represents [D']
        let self_param = ty::mk_param(tcx, num_trait_bounds,
                                      dummy_defid);

        // Represents [E',F',G']
        let num_method_bounds = m.generics.type_param_defs().len();
        let shifted_method_tps = Vec::from_fn(num_method_bounds, |i| {
            ty::mk_param(tcx, i + num_trait_bounds + 1,
                         m.generics.type_param_defs()[i].def_id)
        });

        // Convert the regions 'a, 'b, 'c defined on the trait into
        // bound regions on the fn. Note that because these appear in the
        // bound for `Self` they must be early bound.
        let new_early_region_param_defs = trait_ty_generics.region_param_defs.clone();
        let rps_from_trait =
            trait_ty_generics.region_param_defs().iter().
            enumerate().
            map(|(index,d)| ty::ReEarlyBound(d.def_id.node, index, d.name)).
            collect();

        // build up the substitution from
        //     'a,'b,'c => 'a,'b,'c
        //     A,B,C => A',B',C'
        //     Self => D'
        //     D,E,F => E',F',G'
        let substs = substs {
            regions: ty::NonerasedRegions(rps_from_trait),
            self_ty: Some(self_param),
            tps: non_shifted_trait_tps.append(shifted_method_tps.as_slice())
        };

        // create the type of `foo`, applying the substitution above
        let ty = ty::subst(tcx,
                           &substs,
                           ty::mk_bare_fn(tcx, m.fty.clone()));

        // create the type parameter definitions for `foo`, applying
        // the substitution to any traits that appear in their bounds.

        // add in the type parameters from the trait
        let mut new_type_param_defs = Vec::new();
        let substd_type_param_defs =
            trait_ty_generics.type_param_defs.subst(tcx, &substs);
        new_type_param_defs.push_all(substd_type_param_defs.as_slice());

        // add in the "self" type parameter
        let self_trait_def = get_trait_def(ccx, local_def(trait_id));
        let self_trait_ref = self_trait_def.trait_ref.subst(tcx, &substs);
        new_type_param_defs.push(ty::TypeParameterDef {
            ident: special_idents::self_,
            def_id: dummy_defid,
            bounds: @ty::ParamBounds {
                builtin_bounds: ty::EmptyBuiltinBounds(),
                trait_bounds: vec!(self_trait_ref)
            },
            default: None
        });

        // add in the type parameters from the method
        let substd_type_param_defs = m.generics.type_param_defs.subst(tcx, &substs);
        new_type_param_defs.push_all(substd_type_param_defs.as_slice());

        debug!("static method {} type_param_defs={} ty={}, substs={}",
               m.def_id.repr(tcx),
               new_type_param_defs.repr(tcx),
               ty.repr(tcx),
               substs.repr(tcx));

        tcx.tcache.borrow_mut().insert(m.def_id,
                          ty_param_bounds_and_ty {
                              generics: ty::Generics {
                                  type_param_defs: Rc::new(new_type_param_defs),
                                  region_param_defs: new_early_region_param_defs
                              },
                              ty: ty
                          });
    }

    fn ty_method_of_trait_method(this: &CrateCtxt,
                                 trait_id: ast::NodeId,
                                 trait_generics: &ty::Generics,
                                 m_id: &ast::NodeId,
                                 m_ident: &ast::Ident,
                                 m_explicit_self: &ast::ExplicitSelf,
                                 m_generics: &ast::Generics,
                                 m_fn_style: &ast::FnStyle,
                                 m_decl: &ast::FnDecl) -> ty::Method
    {
        let trait_self_ty = ty::mk_self(this.tcx, local_def(trait_id));
        let fty = astconv::ty_of_method(this, *m_id, *m_fn_style, trait_self_ty,
                                        *m_explicit_self, m_decl);
        let num_trait_type_params = trait_generics.type_param_defs().len();
        let ty_generics = ty_generics_for_fn_or_method(this, m_generics,
                                                       num_trait_type_params);
        ty::Method::new(
            *m_ident,
            ty_generics,
            fty,
            m_explicit_self.node,
            // assume public, because this is only invoked on trait methods
            ast::Public,
            local_def(*m_id),
            TraitContainer(local_def(trait_id)),
            None
        )
    }
}

pub fn ensure_supertraits(ccx: &CrateCtxt,
                          id: ast::NodeId,
                          sp: codemap::Span,
                          ast_trait_refs: &[ast::TraitRef])
                          -> ty::BuiltinBounds
{
    let tcx = ccx.tcx;

    // Called only the first time trait_def_of_item is called.
    // Supertraits are ensured at the same time.
    assert!(!tcx.supertraits.borrow().contains_key(&local_def(id)));

    let self_ty = ty::mk_self(ccx.tcx, local_def(id));
    let mut ty_trait_refs: Vec<@ty::TraitRef> = Vec::new();
    let mut bounds = ty::EmptyBuiltinBounds();
    for ast_trait_ref in ast_trait_refs.iter() {
        let trait_def_id = ty::trait_ref_to_def_id(ccx.tcx, ast_trait_ref);
        // FIXME(#8559): Need to instantiate the trait_ref whether or not it's a
        // builtin trait, so that the trait's node id appears in the tcx trait_ref
        // map. This is only needed for metadata; see the similar fixme in encoder.rs.
        let trait_ref = instantiate_trait_ref(ccx, ast_trait_ref, self_ty);
        if !ty::try_add_builtin_trait(ccx.tcx, trait_def_id, &mut bounds) {

            // FIXME(#5527) Could have same trait multiple times
            if ty_trait_refs.iter().any(|other_trait| other_trait.def_id == trait_ref.def_id) {
                // This means a trait inherited from the same supertrait more
                // than once.
                tcx.sess.span_err(sp, "duplicate supertrait in trait declaration");
                break;
            } else {
                ty_trait_refs.push(trait_ref);
            }
        }
    }

    tcx.supertraits.borrow_mut().insert(local_def(id), @ty_trait_refs);
    bounds
}

pub fn convert_field(ccx: &CrateCtxt,
                     struct_generics: &ty::Generics,
                     v: &ast::StructField,
                     origin: ast::DefId) -> ty::field_ty {
    let tt = ccx.to_ty(&ExplicitRscope, v.node.ty);
    write_ty_to_tcx(ccx.tcx, v.node.id, tt);
    /* add the field to the tcache */
    ccx.tcx.tcache.borrow_mut().insert(local_def(v.node.id),
                                       ty::ty_param_bounds_and_ty {
                                           generics: struct_generics.clone(),
                                           ty: tt
                                       });

    match v.node.kind {
        ast::NamedField(ident, visibility) => {
            ty::field_ty {
                name: ident.name,
                id: local_def(v.node.id),
                vis: visibility,
                origin: origin,
            }
        }
        ast::UnnamedField(visibility) => {
            ty::field_ty {
                name: special_idents::unnamed_field.name,
                id: local_def(v.node.id),
                vis: visibility,
                origin: origin,
            }
        }
    }
}

fn convert_methods(ccx: &CrateCtxt,
                   container: MethodContainer,
                   ms: &[@ast::Method],
                   untransformed_rcvr_ty: ty::t,
                   rcvr_ty_generics: &ty::Generics,
                   rcvr_ast_generics: &ast::Generics,
                   rcvr_visibility: ast::Visibility)
{
    let tcx = ccx.tcx;
    let mut seen_methods = HashSet::new();
    for m in ms.iter() {
        if !seen_methods.insert(m.ident.repr(ccx.tcx)) {
            tcx.sess.span_err(m.span, "duplicate method in trait impl");
        }

        let num_rcvr_ty_params = rcvr_ty_generics.type_param_defs().len();
        let m_ty_generics = ty_generics_for_fn_or_method(ccx, &m.generics,
                                                         num_rcvr_ty_params);
        let mty = @ty_of_method(ccx,
                                container,
                                *m,
                                untransformed_rcvr_ty,
                                rcvr_ast_generics,
                                rcvr_visibility);
        let fty = ty::mk_bare_fn(tcx, mty.fty.clone());
        debug!("method {} (id {}) has type {}",
                m.ident.repr(ccx.tcx),
                m.id,
                fty.repr(ccx.tcx));
        tcx.tcache.borrow_mut().insert(
            local_def(m.id),

            // n.b.: the type of a method is parameterized by both
            // the parameters on the receiver and those on the method
            // itself
            ty_param_bounds_and_ty {
                generics: ty::Generics {
                    type_param_defs: Rc::new(Vec::from_slice(rcvr_ty_generics.type_param_defs())
                                             .append(m_ty_generics.type_param_defs())),
                    region_param_defs: Rc::new(Vec::from_slice(rcvr_ty_generics.region_param_defs())
                                               .append(m_ty_generics.region_param_defs())),
                },
                ty: fty
            });

        write_ty_to_tcx(tcx, m.id, fty);

        tcx.methods.borrow_mut().insert(mty.def_id, mty);
    }

    fn ty_of_method(ccx: &CrateCtxt,
                    container: MethodContainer,
                    m: &ast::Method,
                    untransformed_rcvr_ty: ty::t,
                    rcvr_generics: &ast::Generics,
                    rcvr_visibility: ast::Visibility) -> ty::Method
    {
        let fty = astconv::ty_of_method(ccx, m.id, m.fn_style,
                                        untransformed_rcvr_ty,
                                        m.explicit_self, m.decl);

        // if the method specifies a visibility, use that, otherwise
        // inherit the visibility from the impl (so `foo` in `pub impl
        // { fn foo(); }` is public, but private in `priv impl { fn
        // foo(); }`).
        let method_vis = m.vis.inherit_from(rcvr_visibility);

        let num_rcvr_type_params = rcvr_generics.ty_params.len();
        let m_ty_generics =
            ty_generics_for_fn_or_method(ccx, &m.generics, num_rcvr_type_params);
        ty::Method::new(
            m.ident,
            m_ty_generics,
            fty,
            m.explicit_self.node,
            method_vis,
            local_def(m.id),
            container,
            None
        )
    }
}

pub fn ensure_no_ty_param_bounds(ccx: &CrateCtxt,
                                 span: Span,
                                 generics: &ast::Generics,
                                 thing: &'static str) {
    for ty_param in generics.ty_params.iter() {
        if ty_param.bounds.len() > 0 {
            ccx.tcx.sess.span_err(
                span,
                format!("trait bounds are not allowed in {} definitions",
                     thing));
        }
    }
}

fn ensure_generics_abi(ccx: &CrateCtxt,
                       span: Span,
                       abi: abi::Abi,
                       generics: &ast::Generics) {
    if generics.ty_params.len() > 0 &&
       !(abi == abi::Rust || abi == abi::RustIntrinsic) {
        ccx.tcx.sess.span_err(span,
                              "foreign functions may not use type parameters");
    }
}

pub fn convert(ccx: &CrateCtxt, it: &ast::Item) {
    let tcx = ccx.tcx;
    debug!("convert: item {} with id {}", token::get_ident(it.ident), it.id);
    match it.node {
        // These don't define types.
        ast::ItemForeignMod(_) | ast::ItemMod(_) | ast::ItemMac(_) => {}
        ast::ItemEnum(ref enum_definition, ref generics) => {
            ensure_no_ty_param_bounds(ccx, it.span, generics, "enumeration");
            let tpt = ty_of_item(ccx, it);
            write_ty_to_tcx(tcx, it.id, tpt.ty);
            get_enum_variant_types(ccx,
                                   tpt.ty,
                                   enum_definition.variants.as_slice(),
                                   generics);
        },
        ast::ItemImpl(ref generics, ref opt_trait_ref, selfty, ref ms) => {
            let ty_generics = ty_generics_for_type(ccx, generics);
            let selfty = ccx.to_ty(&ExplicitRscope, selfty);
            write_ty_to_tcx(tcx, it.id, selfty);

            tcx.tcache.borrow_mut().insert(local_def(it.id),
                                ty_param_bounds_and_ty {
                                    generics: ty_generics.clone(),
                                    ty: selfty});

            // If there is a trait reference, treat the methods as always public.
            // This is to work around some incorrect behavior in privacy checking:
            // when the method belongs to a trait, it should acquire the privacy
            // from the trait, not the impl. Forcing the visibility to be public
            // makes things sorta work.
            let parent_visibility = if opt_trait_ref.is_some() {
                ast::Public
            } else {
                it.vis
            };

            convert_methods(ccx,
                            ImplContainer(local_def(it.id)),
                            ms.as_slice(),
                            selfty,
                            &ty_generics,
                            generics,
                            parent_visibility);

            for trait_ref in opt_trait_ref.iter() {
                let trait_ref = instantiate_trait_ref(ccx, trait_ref, selfty);

                // Prevent the builtin kind traits from being manually implemented.
                if tcx.lang_items.to_builtin_kind(trait_ref.def_id).is_some() {
                    tcx.sess.span_err(it.span,
                        "cannot provide an explicit implementation \
                         for a builtin kind");
                }
            }
        },
        ast::ItemTrait(ref generics, _, ref trait_methods) => {
            let trait_def = trait_def_of_item(ccx, it);

            // Run convert_methods on the provided methods.
            let (_, provided_methods) =
                split_trait_methods(trait_methods.as_slice());
            let untransformed_rcvr_ty = ty::mk_self(tcx, local_def(it.id));
            convert_methods(ccx,
                            TraitContainer(local_def(it.id)),
                            provided_methods.as_slice(),
                            untransformed_rcvr_ty,
                            &trait_def.generics,
                            generics,
                            it.vis);

            // We need to do this *after* converting methods, since
            // convert_methods produces a tcache entry that is wrong for
            // static trait methods. This is somewhat unfortunate.
            ensure_trait_methods(ccx, it.id);
        },
        ast::ItemStruct(struct_def, ref generics) => {
            ensure_no_ty_param_bounds(ccx, it.span, generics, "structure");

            // Write the class type.
            let tpt = ty_of_item(ccx, it);
            write_ty_to_tcx(tcx, it.id, tpt.ty);

            tcx.tcache.borrow_mut().insert(local_def(it.id), tpt.clone());

            // Write the super-struct type, if it exists.
            match struct_def.super_struct {
                Some(ty) => {
                    let supserty = ccx.to_ty(&ExplicitRscope, ty);
                    write_ty_to_tcx(tcx, it.id, supserty);
                },
                _ => {},
            }

            convert_struct(ccx, struct_def, tpt, it.id);
        },
        ast::ItemTy(_, ref generics) => {
            ensure_no_ty_param_bounds(ccx, it.span, generics, "type");
            let tpt = ty_of_item(ccx, it);
            write_ty_to_tcx(tcx, it.id, tpt.ty);
        },
        ast::ItemFn(_, _, abi, ref generics, _) => {
            ensure_generics_abi(ccx, it.span, abi, generics);
            let tpt = ty_of_item(ccx, it);
            write_ty_to_tcx(tcx, it.id, tpt.ty);
        },
        _ => {
            // This call populates the type cache with the converted type
            // of the item in passing. All we have to do here is to write
            // it into the node type table.
            let tpt = ty_of_item(ccx, it);
            write_ty_to_tcx(tcx, it.id, tpt.ty);
        },
    }
}

pub fn convert_struct(ccx: &CrateCtxt,
                      struct_def: &ast::StructDef,
                      tpt: ty::ty_param_bounds_and_ty,
                      id: ast::NodeId) {
    let tcx = ccx.tcx;

    // Write the type of each of the members and check for duplicate fields.
    let mut seen_fields: HashMap<ast::Name, Span> = HashMap::new();
    let field_tys = struct_def.fields.iter().map(|f| {
        let result = convert_field(ccx, &tpt.generics, f, local_def(id));

        if result.name != special_idents::unnamed_field.name {
            let dup = match seen_fields.find(&result.name) {
                Some(prev_span) => {
                    tcx.sess.span_err(f.span,
                        format!("field `{}` is already declared", token::get_name(result.name)));
                    tcx.sess.span_note(*prev_span,
                        "previously declared here");
                    true
                },
                None => false,
            };
            // FIXME(#6393) this whole dup thing is just to satisfy
            // the borrow checker :-(
            if !dup {
                seen_fields.insert(result.name, f.span);
            }
        }

        result
    }).collect();

    tcx.struct_fields.borrow_mut().insert(local_def(id), @field_tys);

    let super_struct = match struct_def.super_struct {
        Some(t) => match t.node {
            ast::TyPath(_, _, path_id) => {
                let def_map = tcx.def_map.borrow();
                match def_map.find(&path_id) {
                    Some(&ast::DefStruct(def_id)) => {
                        // FIXME(#12511) Check for cycles in the inheritance hierarchy.
                        // Check super-struct is virtual.
                        match tcx.map.find(def_id.node) {
                            Some(ast_map::NodeItem(i)) => match i.node {
                                ast::ItemStruct(struct_def, _) => {
                                    if !struct_def.is_virtual {
                                        tcx.sess.span_err(t.span,
                                            "struct inheritance is only \
                                             allowed from virtual structs");
                                    }
                                },
                                _ => {},
                            },
                            _ => {},
                        }

                        Some(def_id)
                    },
                    _ => None,
                }
            }
            _ => None,
        },
        None => None,
    };
    tcx.superstructs.borrow_mut().insert(local_def(id), super_struct);

    let substs = mk_item_substs(ccx, &tpt.generics, None);
    let selfty = ty::mk_struct(tcx, local_def(id), substs);

    // If this struct is enum-like or tuple-like, create the type of its
    // constructor.
    match struct_def.ctor_id {
        None => {}
        Some(ctor_id) => {
            if struct_def.fields.len() == 0 {
                // Enum-like.
                write_ty_to_tcx(tcx, ctor_id, selfty);

                tcx.tcache.borrow_mut().insert(local_def(ctor_id), tpt);
            } else if struct_def.fields.get(0).node.kind.is_unnamed() {
                // Tuple-like.
                let inputs: Vec<_> = struct_def.fields.iter().map(
                        |field| tcx.tcache.borrow().get(
                            &local_def(field.node.id)).ty).collect();
                let ctor_fn_ty = ty::mk_ctor_fn(tcx,
                                                ctor_id,
                                                inputs.as_slice(),
                                                selfty);
                write_ty_to_tcx(tcx, ctor_id, ctor_fn_ty);
                tcx.tcache.borrow_mut().insert(local_def(ctor_id),
                                  ty_param_bounds_and_ty {
                    generics: tpt.generics,
                    ty: ctor_fn_ty
                });
            }
        }
    }
}

pub fn convert_foreign(ccx: &CrateCtxt, i: &ast::ForeignItem) {
    // As above, this call populates the type table with the converted
    // type of the foreign item. We simply write it into the node type
    // table.

    // For reasons I cannot fully articulate, I do so hate the AST
    // map, and I regard each time that I use it as a personal and
    // moral failing, but at the moment it seems like the only
    // convenient way to extract the ABI. - ndm
    let abi = ccx.tcx.map.get_foreign_abi(i.id);

    let tpt = ty_of_foreign_item(ccx, i, abi);
    write_ty_to_tcx(ccx.tcx, i.id, tpt.ty);

    ccx.tcx.tcache.borrow_mut().insert(local_def(i.id), tpt);
}

pub fn instantiate_trait_ref(ccx: &CrateCtxt,
                             ast_trait_ref: &ast::TraitRef,
                             self_ty: ty::t) -> @ty::TraitRef
{
    /*!
     * Instantiates the path for the given trait reference, assuming that
     * it's bound to a valid trait type. Returns the def_id for the defining
     * trait. Fails if the type is a type other than a trait type.
     */

    // FIXME(#5121) -- distinguish early vs late lifetime params
    let rscope = ExplicitRscope;

    match lookup_def_tcx(ccx.tcx, ast_trait_ref.path.span, ast_trait_ref.ref_id) {
        ast::DefTrait(trait_did) => {
            let trait_ref =
                astconv::ast_path_to_trait_ref(
                    ccx, &rscope, trait_did, Some(self_ty), &ast_trait_ref.path);

            ccx.tcx.trait_refs.borrow_mut().insert(ast_trait_ref.ref_id,
                                                   trait_ref);
            return trait_ref;
        }
        _ => {
            ccx.tcx.sess.span_fatal(
                ast_trait_ref.path.span,
                format!("`{}` is not a trait",
                    path_to_str(&ast_trait_ref.path)));
        }
    }
}

fn get_trait_def(ccx: &CrateCtxt, trait_id: ast::DefId) -> @ty::TraitDef {
    if trait_id.krate != ast::LOCAL_CRATE {
        return ty::lookup_trait_def(ccx.tcx, trait_id)
    }

    match ccx.tcx.map.get(trait_id.node) {
        ast_map::NodeItem(item) => trait_def_of_item(ccx, item),
        _ => ccx.tcx.sess.bug(format!("get_trait_def({}): not an item",
                                   trait_id.node))
    }
}

pub fn trait_def_of_item(ccx: &CrateCtxt, it: &ast::Item) -> @ty::TraitDef {
    let def_id = local_def(it.id);
    let tcx = ccx.tcx;
    match tcx.trait_defs.borrow().find(&def_id) {
        Some(&def) => return def,
        _ => {}
    }

    match it.node {
        ast::ItemTrait(ref generics, ref supertraits, _) => {
            let self_ty = ty::mk_self(tcx, def_id);
            let ty_generics = ty_generics_for_type(ccx, generics);
            let substs = mk_item_substs(ccx, &ty_generics, Some(self_ty));
            let bounds = ensure_supertraits(ccx,
                                            it.id,
                                            it.span,
                                            supertraits.as_slice());
            let trait_ref = @ty::TraitRef {def_id: def_id,
                                           substs: substs};
            let trait_def = @ty::TraitDef {generics: ty_generics,
                                           bounds: bounds,
                                           trait_ref: trait_ref};
            tcx.trait_defs.borrow_mut().insert(def_id, trait_def);
            return trait_def;
        }
        ref s => {
            tcx.sess.span_bug(
                it.span,
                format!("trait_def_of_item invoked on {:?}", s));
        }
    }
}

pub fn ty_of_item(ccx: &CrateCtxt, it: &ast::Item)
                  -> ty::ty_param_bounds_and_ty {
    let def_id = local_def(it.id);
    let tcx = ccx.tcx;
    match tcx.tcache.borrow().find(&def_id) {
        Some(tpt) => return tpt.clone(),
        _ => {}
    }
    match it.node {
        ast::ItemStatic(t, _, _) => {
            let typ = ccx.to_ty(&ExplicitRscope, t);
            let tpt = no_params(typ);

            tcx.tcache.borrow_mut().insert(local_def(it.id), tpt.clone());
            return tpt;
        }
        ast::ItemFn(decl, fn_style, abi, ref generics, _) => {
            let ty_generics = ty_generics_for_fn_or_method(ccx, generics, 0);
            let tofd = astconv::ty_of_bare_fn(ccx,
                                              it.id,
                                              fn_style,
                                              abi,
                                              decl);
            let tpt = ty_param_bounds_and_ty {
                generics: ty_generics,
                ty: ty::mk_bare_fn(ccx.tcx, tofd)
            };
            debug!("type of {} (id {}) is {}",
                    token::get_ident(it.ident),
                    it.id,
                    ppaux::ty_to_str(tcx, tpt.ty));

            ccx.tcx.tcache.borrow_mut().insert(local_def(it.id), tpt.clone());
            return tpt;
        }
        ast::ItemTy(t, ref generics) => {
            match tcx.tcache.borrow_mut().find(&local_def(it.id)) {
                Some(tpt) => return tpt.clone(),
                None => { }
            }

            let tpt = {
                let ty = ccx.to_ty(&ExplicitRscope, t);
                ty_param_bounds_and_ty {
                    generics: ty_generics_for_type(ccx, generics),
                    ty: ty
                }
            };

            tcx.tcache.borrow_mut().insert(local_def(it.id), tpt.clone());
            return tpt;
        }
        ast::ItemEnum(_, ref generics) => {
            // Create a new generic polytype.
            let ty_generics = ty_generics_for_type(ccx, generics);
            let substs = mk_item_substs(ccx, &ty_generics, None);
            let t = ty::mk_enum(tcx, local_def(it.id), substs);
            let tpt = ty_param_bounds_and_ty {
                generics: ty_generics,
                ty: t
            };

            tcx.tcache.borrow_mut().insert(local_def(it.id), tpt.clone());
            return tpt;
        }
        ast::ItemTrait(..) => {
            tcx.sess.span_bug(
                it.span,
                format!("invoked ty_of_item on trait"));
        }
        ast::ItemStruct(_, ref generics) => {
            let ty_generics = ty_generics_for_type(ccx, generics);
            let substs = mk_item_substs(ccx, &ty_generics, None);
            let t = ty::mk_struct(tcx, local_def(it.id), substs);
            let tpt = ty_param_bounds_and_ty {
                generics: ty_generics,
                ty: t
            };

            tcx.tcache.borrow_mut().insert(local_def(it.id), tpt.clone());
            return tpt;
        }
        ast::ItemImpl(..) | ast::ItemMod(_) |
        ast::ItemForeignMod(_) | ast::ItemMac(_) => fail!(),
    }
}

pub fn ty_of_foreign_item(ccx: &CrateCtxt,
                          it: &ast::ForeignItem,
                          abi: abi::Abi) -> ty::ty_param_bounds_and_ty
{
    match it.node {
        ast::ForeignItemFn(fn_decl, ref generics) => {
            ty_of_foreign_fn_decl(ccx,
                                  fn_decl,
                                  local_def(it.id),
                                  generics,
                                  abi)
        }
        ast::ForeignItemStatic(t, _) => {
            ty::ty_param_bounds_and_ty {
                generics: ty::Generics {
                    type_param_defs: Rc::new(Vec::new()),
                    region_param_defs: Rc::new(Vec::new()),
                },
                ty: ast_ty_to_ty(ccx, &ExplicitRscope, t)
            }
        }
    }
}

pub fn ty_generics_for_type(ccx: &CrateCtxt,
                            generics: &ast::Generics)
                            -> ty::Generics {
    ty_generics(ccx, &generics.lifetimes, &generics.ty_params, 0)
}

pub fn ty_generics_for_fn_or_method(ccx: &CrateCtxt,
                                    generics: &ast::Generics,
                                    base_index: uint)
                                    -> ty::Generics {
    let early_lifetimes = resolve_lifetime::early_bound_lifetimes(generics);
    ty_generics(ccx, &early_lifetimes, &generics.ty_params, base_index)
}

pub fn ty_generics(ccx: &CrateCtxt,
                   lifetimes: &Vec<ast::Lifetime>,
                   ty_params: &OwnedSlice<ast::TyParam>,
                   base_index: uint) -> ty::Generics {
    return ty::Generics {
        region_param_defs: Rc::new(lifetimes.iter().map(|l| {
                ty::RegionParameterDef { name: l.name,
                                         def_id: local_def(l.id) }
            }).collect()),
        type_param_defs: Rc::new(ty_params.iter().enumerate().map(|(offset, param)| {
            let existing_def_opt = {
                let ty_param_defs = ccx.tcx.ty_param_defs.borrow();
                ty_param_defs.find(&param.id).map(|&def| def)
            };
            existing_def_opt.unwrap_or_else(|| {
                let param_ty = ty::param_ty {idx: base_index + offset,
                                             def_id: local_def(param.id)};
                let bounds = @compute_bounds(ccx, param_ty, &param.bounds);
                let default = param.default.map(|path| {
                    let ty = ast_ty_to_ty(ccx, &ExplicitRscope, path);
                    let cur_idx = param_ty.idx;

                    ty::walk_ty(ty, |t| {
                        match ty::get(t).sty {
                            ty::ty_param(p) => if p.idx > cur_idx {
                                ccx.tcx.sess.span_err(path.span,
                                                        "type parameters with a default cannot use \
                                                        forward declared identifiers")
                            },
                            _ => {}
                        }
                    });

                    ty
                });

                let def = ty::TypeParameterDef {
                    ident: param.ident,
                    def_id: local_def(param.id),
                    bounds: bounds,
                    default: default
                };
                debug!("def for param: {}", def.repr(ccx.tcx));
                ccx.tcx.ty_param_defs.borrow_mut().insert(param.id, def);
                def
            })
        }).collect()),
    };

    fn compute_bounds(
        ccx: &CrateCtxt,
        param_ty: ty::param_ty,
        ast_bounds: &OwnedSlice<ast::TyParamBound>) -> ty::ParamBounds
    {
        /*!
         * Translate the AST's notion of ty param bounds (which are an
         * enum consisting of a newtyped Ty or a region) to ty's
         * notion of ty param bounds, which can either be user-defined
         * traits, or the built-in trait (formerly known as kind): Send.
         */

        let mut param_bounds = ty::ParamBounds {
            builtin_bounds: ty::EmptyBuiltinBounds(),
            trait_bounds: Vec::new()
        };
        for ast_bound in ast_bounds.iter() {
            match *ast_bound {
                TraitTyParamBound(ref b) => {
                    let ty = ty::mk_param(ccx.tcx, param_ty.idx, param_ty.def_id);
                    let trait_ref = instantiate_trait_ref(ccx, b, ty);
                    if !ty::try_add_builtin_trait(
                        ccx.tcx, trait_ref.def_id,
                        &mut param_bounds.builtin_bounds)
                    {
                        // Must be a user-defined trait
                        param_bounds.trait_bounds.push(trait_ref);
                    }
                }

                RegionTyParamBound => {
                    param_bounds.builtin_bounds.add(ty::BoundStatic);
                }
            }
        }

        param_bounds
    }
}

pub fn ty_of_foreign_fn_decl(ccx: &CrateCtxt,
                             decl: &ast::FnDecl,
                             def_id: ast::DefId,
                             ast_generics: &ast::Generics,
                             abi: abi::Abi)
                          -> ty::ty_param_bounds_and_ty {

    for i in decl.inputs.iter() {
        match (*i).pat.node {
            ast::PatIdent(_, _, _) => (),
            ast::PatWild => (),
            _ => ccx.tcx.sess.span_err((*i).pat.span,
                    "patterns aren't allowed in foreign function declarations")
        }
    }

    let ty_generics_for_fn_or_method =
        ty_generics_for_fn_or_method(ccx, ast_generics, 0);
    let rb = BindingRscope::new(def_id.node);
    let input_tys = decl.inputs
                        .iter()
                        .map(|a| ty_of_arg(ccx, &rb, a, None))
                        .collect();

    let output_ty = ast_ty_to_ty(ccx, &rb, decl.output);

    let t_fn = ty::mk_bare_fn(
        ccx.tcx,
        ty::BareFnTy {
            abi: abi,
            fn_style: ast::UnsafeFn,
            sig: ty::FnSig {binder_id: def_id.node,
                            inputs: input_tys,
                            output: output_ty,
                            variadic: decl.variadic}
        });
    let tpt = ty_param_bounds_and_ty {
        generics: ty_generics_for_fn_or_method,
        ty: t_fn
    };

    ccx.tcx.tcache.borrow_mut().insert(def_id, tpt.clone());
    return tpt;
}

pub fn mk_item_substs(ccx: &CrateCtxt,
                      ty_generics: &ty::Generics,
                      self_ty: Option<ty::t>) -> ty::substs
{
    let params: Vec<ty::t> =
        ty_generics.type_param_defs().iter().enumerate().map(
            |(i, t)| ty::mk_param(ccx.tcx, i, t.def_id)).collect();

    let regions: OwnedSlice<ty::Region> =
        ty_generics.region_param_defs().iter().enumerate().map(
            |(i, l)| ty::ReEarlyBound(l.def_id.node, i, l.name)).collect();

    substs {regions: ty::NonerasedRegions(regions),
            self_ty: self_ty,
            tps: params}
}
