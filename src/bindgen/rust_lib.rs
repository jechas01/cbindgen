/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use syn;

use bindgen::cargo::{Cargo, PackageRef};
use bindgen::ir::Cfg;

const STD_CRATES: &'static [&'static str] = &["std",
                                              "std_unicode",
                                              "alloc",
                                              "collections",
                                              "core",
                                              "proc_macro"];

type ParseResult = Result<(), String>;

/// Parses a single rust source file, not following `mod` or `extern crate`.
pub fn parse_src<F>(src_file: &Path,
                    items_callback: &mut F) -> ParseResult
    where F: FnMut(&str, &Vec<syn::Item>)
{
    let src_parsed = {
        let mut s = String::new();
        let mut f = File::open(src_file).map_err(|_| format!("Parsing: cannot open file `{:?}`.", src_file))?;
        f.read_to_string(&mut s).map_err(|_| format!("Parsing: cannot open file `{:?}`.", src_file))?;
        syn::parse_crate(&s).map_err(|msg| format!("Parsing:\n{}.", msg))?
    };

    items_callback("", &src_parsed.items);

    Ok(())
}

/// Recursively parses a rust library starting at the root crate's directory.
///
/// Inside a crate, `mod` and `extern crate` declarations are followed
/// and parsed. To find an external crate, the parser uses the `cargo metadata`
/// command to find the location of dependencies.
pub fn parse_lib<F>(lib: Cargo,
                    parse_deps: bool,
                    include: &Option<Vec<String>>,
                    exclude: &Vec<String>,
                    expand: &Vec<String>,
                    items_callback: &mut F) -> ParseResult
    where F: FnMut(&str, &str, Option<Cfg>, &Vec<syn::Item>)
{
    let mut context = ParseLibContext {
        lib: lib,
        parse_deps: parse_deps,
        include: include.clone(),
        exclude: exclude.clone(),
        expand: expand.clone(),
        parsed_crates: HashSet::new(),
        cache_src: HashMap::new(),
        cache_expanded_crate: HashMap::new(),

        cfg_stack: Vec::new(),

        items_callback: items_callback,
    };

    parse_crate(&context.lib.binding_crate_ref(), &mut context)
}

struct ParseLibContext<F>
    where F: FnMut(&str, &str, Option<Cfg>, &Vec<syn::Item>)
{
    lib: Cargo,
    parse_deps: bool,

    include: Option<Vec<String>>,
    exclude: Vec<String>,
    expand: Vec<String>,

    parsed_crates: HashSet<String>,
    cache_src: HashMap<PathBuf, Vec<syn::Item>>,
    cache_expanded_crate: HashMap<String, Vec<syn::Item>>,

    cfg_stack: Vec<Cfg>,

    items_callback: F,
}

impl<F> ParseLibContext<F>
    where F: FnMut(&str, &str, Option<Cfg>, &Vec<syn::Item>)
{
    fn should_parse_dependency(&self, pkg_name: &String) -> bool {
        if self.parsed_crates.contains(pkg_name) {
            return false;
        }

        if !self.parse_deps {
            return false;
        }

        // Skip any whitelist or blacklist for expand
        if self.expand.contains(&pkg_name) {
            return true;
        }

        // If we have a whitelist, check it
        if let Some(ref include) = self.include {
            if !include.contains(&pkg_name) {
                return false;
            }
        }

        // Check the blacklist
        return !STD_CRATES.contains(&pkg_name.as_ref()) &&
               !self.exclude.contains(&pkg_name);
    }
}

fn parse_crate<F>(pkg: &PackageRef, context: &mut ParseLibContext<F>) -> ParseResult
    where F: FnMut(&str, &str, Option<Cfg>, &Vec<syn::Item>)
{
    context.parsed_crates.insert(pkg.name.clone());

    // Check if we should use cargo expand for this crate
    if context.expand.contains(&pkg.name) {
        return parse_expand_crate(pkg, context);
    }

    // Otherwise do our normal parse
    let crate_src = context.lib.find_crate_src(pkg);

    match crate_src {
        Some(crate_src) => {
            parse_mod(pkg, crate_src.as_path(), context)
        },
        None => {
            // This should be an error, but is common enough to just elicit a warning
            warn!("Parsing crate `{}`: can't find lib.rs with `cargo metadata`.", pkg.name);
            Ok(())
        },
    }
}

fn parse_expand_crate<F>(pkg: &PackageRef, context: &mut ParseLibContext<F>) -> ParseResult
    where F: FnMut(&str, &str, Option<Cfg>, &Vec<syn::Item>)
{
    let mod_parsed = {
        if !context.cache_expanded_crate.contains_key(&pkg.name) {
            let s = context.lib.expand_crate(pkg)?;
            let i = syn::parse_crate(&s).map_err(|msg| format!("Parsing crate `{}`:\n{}.", pkg.name, msg))?;
            context.cache_expanded_crate.insert(pkg.name.clone(), i.items);
        }

        context.cache_expanded_crate.get(&pkg.name).unwrap().clone()
    };

    process_expanded_mod(pkg, &mod_parsed, context)
}

fn process_expanded_mod<F>(pkg: &PackageRef,
                           items: &Vec<syn::Item>,
                           context: &mut ParseLibContext<F>) -> ParseResult
    where F: FnMut(&str, &str, Option<Cfg>, &Vec<syn::Item>)
{
    (context.items_callback)(context.lib.binding_crate_name(),
                             &pkg.name,
                             Cfg::join(&context.cfg_stack),
                             items);

    for item in items {
        match item.node {
            syn::ItemKind::Mod(ref inline_items) => {
                let cfg = Cfg::load(&item.attrs);
                if let &Some(ref cfg) = &cfg {
                    context.cfg_stack.push(cfg.clone());
                }

                if let &Some(ref inline_items) = inline_items {
                    process_expanded_mod(pkg, inline_items, context)?;
                } else {
                    error!("Parsing crate `{}`: external mod found in expanded source, this shouldn't be possible.", pkg.name);
                }

                if cfg.is_some() {
                    context.cfg_stack.pop();
                }
            }
            syn::ItemKind::ExternCrate(_) => {
                let dep_pkg_name = item.ident.to_string();

                let cfg = Cfg::load(&item.attrs);
                if let &Some(ref cfg) = &cfg {
                    context.cfg_stack.push(cfg.clone());
                }

                if context.should_parse_dependency(&dep_pkg_name) {
                    let dep_pkg_ref = context.lib.find_dep_ref(pkg, &dep_pkg_name);

                    if let Some(dep_pkg_ref) = dep_pkg_ref {
                        parse_crate(&dep_pkg_ref, context)?;
                    } else {
                        error!("Parsing crate `{}`: can't find dependency version for {}`.", pkg.name, dep_pkg_name);
                    }
                }

                if cfg.is_some() {
                    context.cfg_stack.pop();
                }
            }
            _ => {}
        }
    }

    Ok(())
}

fn parse_mod<F>(pkg: &PackageRef,
                mod_path: &Path,
                context: &mut ParseLibContext<F>) -> ParseResult
    where F: FnMut(&str, &str, Option<Cfg>, &Vec<syn::Item>)
{
    let mod_parsed = {
        let owned_mod_path = mod_path.to_path_buf();

        if !context.cache_src.contains_key(&owned_mod_path) {
            let mut s = String::new();
            let mut f = File::open(mod_path).map_err(|_| format!("Parsing crate `{}`: cannot open file `{:?}`.", pkg.name, mod_path))?;
            f.read_to_string(&mut s).map_err(|_| format!("Parsing crate `{}`: cannot open file `{:?}`.", pkg.name, mod_path))?;
            let i = syn::parse_crate(&s).map_err(|msg| format!("Parsing crate `{}`:\n{}.", pkg.name, msg))?;
            context.cache_src.insert(owned_mod_path.clone(), i.items);
        }

        context.cache_src.get(&owned_mod_path).unwrap().clone()
    };

    let mod_dir = mod_path.parent().unwrap();

    process_mod(pkg,
                mod_dir,
                &mod_parsed,
                context)
}

fn process_mod<F>(pkg: &PackageRef,
                  mod_dir: &Path,
                  items: &Vec<syn::Item>,
                  context: &mut ParseLibContext<F>) -> ParseResult
    where F: FnMut(&str, &str, Option<Cfg>, &Vec<syn::Item>)
{
    (context.items_callback)(context.lib.binding_crate_name(),
                             &pkg.name,
                             Cfg::join(&context.cfg_stack),
                             items);

    for item in items {
        match item.node {
            syn::ItemKind::Mod(ref inline_items) => {
                let next_mod_name = item.ident.to_string();

                let cfg = Cfg::load(&item.attrs);
                if let &Some(ref cfg) = &cfg {
                    context.cfg_stack.push(cfg.clone());
                }

                if let &Some(ref inline_items) = inline_items {
                    process_mod(pkg,
                                &mod_dir.join(&next_mod_name),
                                inline_items,
                                context)?;
                } else {
                    let next_mod_path1 = mod_dir.join(next_mod_name.clone() + ".rs");
                    let next_mod_path2 = mod_dir.join(next_mod_name.clone()).join("mod.rs");

                    if next_mod_path1.exists() {
                        parse_mod(pkg,
                                  next_mod_path1.as_path(),
                                  context)?;
                    } else if next_mod_path2.exists() {
                        parse_mod(pkg,
                                  next_mod_path2.as_path(),
                                  context)?;
                    } else {
                        // This should be an error, but is common enough to just elicit a warning
                        warn!("Parsing crate `{}`: can't find mod {}`.", pkg.name, next_mod_name);
                    }
                }

                if cfg.is_some() {
                    context.cfg_stack.pop();
                }
            }
            syn::ItemKind::ExternCrate(ref cr) => {
                let dep_pkg_name = if let Some(ref name) = *cr {
                    name.to_string()
                } else {
                    item.ident.to_string()
                };

                let cfg = Cfg::load(&item.attrs);
                if let &Some(ref cfg) = &cfg {
                    context.cfg_stack.push(cfg.clone());
                }

                if context.should_parse_dependency(&dep_pkg_name) {
                    let dep_pkg_ref = context.lib.find_dep_ref(pkg, &dep_pkg_name);

                    if let Some(dep_pkg_ref) = dep_pkg_ref {
                        parse_crate(&dep_pkg_ref, context)?;
                    } else {
                        error!("Parsing crate `{}`: can't find dependency version for {}`.", pkg.name, dep_pkg_name);
                    }
                }

                if cfg.is_some() {
                    context.cfg_stack.pop();
                }
            }
            _ => {}
        }
    }

    Ok(())
}
