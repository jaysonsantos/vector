use regex::Regex;
use vrl::{function::Error, prelude::*};

use crate::util;

fn parse_regex_all(value: Value, numeric_groups: bool, pattern: &Regex) -> Resolved {
    let bytes = value.try_bytes()?;
    let value = String::from_utf8_lossy(&bytes);
    Ok(pattern
        .captures_iter(&value)
        .map(|capture| util::capture_regex_to_map(pattern, capture, numeric_groups).into())
        .collect::<Vec<Value>>()
        .into())
}

#[derive(Clone, Copy, Debug)]
pub struct ParseRegexAll;

impl Function for ParseRegexAll {
    fn identifier(&self) -> &'static str {
        "parse_regex_all"
    }

    fn parameters(&self) -> &'static [Parameter] {
        &[
            Parameter {
                keyword: "value",
                kind: kind::ANY,
                required: true,
            },
            Parameter {
                keyword: "pattern",
                kind: kind::ANY,
                required: true,
            },
            Parameter {
                keyword: "numeric_groups",
                kind: kind::BOOLEAN,
                required: false,
            },
        ]
    }

    fn compile(
        &self,
        _state: (&mut state::LocalEnv, &mut state::ExternalEnv),
        _ctx: &mut FunctionCompileContext,
        mut arguments: ArgumentList,
    ) -> Compiled {
        let value = arguments.required("value");
        let pattern = arguments.required_regex("pattern")?;
        let numeric_groups = arguments
            .optional("numeric_groups")
            .unwrap_or_else(|| expr!(false));

        Ok(Box::new(ParseRegexAllFn {
            value,
            pattern,
            numeric_groups,
        }))
    }

    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                title: "Simple match",
                source: r#"parse_regex_all!("apples and carrots, peaches and peas", r'(?P<fruit>[\w\.]+) and (?P<veg>[\w]+)')"#,
                result: Ok(indoc! { r#"[
               {"fruit": "apples",
                "veg": "carrots"},
               {"fruit": "peaches",
                "veg": "peas"}]"# }),
            },
            Example {
                title: "Numeric groups",
                source: r#"parse_regex_all!("apples and carrots, peaches and peas", r'(?P<fruit>[\w\.]+) and (?P<veg>[\w]+)', numeric_groups: true)"#,
                result: Ok(indoc! { r#"[
               {"fruit": "apples",
                "veg": "carrots",
                "0": "apples and carrots",
                "1": "apples",
                "2": "carrots"},
               {"fruit": "peaches",
                "veg": "peas",
                "0": "peaches and peas",
                "1": "peaches",
                "2": "peas"}]"# }),
            },
        ]
    }

    fn compile_argument(
        &self,
        _args: &[(&'static str, Option<FunctionArgument>)],
        _ctx: &mut FunctionCompileContext,
        name: &str,
        expr: Option<&expression::Expr>,
    ) -> CompiledArgument {
        match (name, expr) {
            ("pattern", Some(expr)) => {
                let regex: regex::Regex = match expr {
                    expression::Expr::Literal(expression::Literal::Regex(regex)) => {
                        Ok((**regex).clone())
                    }
                    expr => Err(Error::UnexpectedExpression {
                        keyword: "pattern",
                        expected: "regex",
                        expr: expr.clone(),
                    }),
                }?;

                Ok(Some(Box::new(regex) as _))
            }
            _ => Ok(None),
        }
    }

    fn call_by_vm(&self, _ctx: &mut Context, args: &mut VmArgumentList) -> Resolved {
        let pattern = args
            .required_any("pattern")
            .downcast_ref::<regex::Regex>()
            .ok_or("no pattern")?;
        let value = args.required("value");
        let numeric_groups = args
            .optional("numeric_groups")
            .map(|value| value.try_boolean())
            .transpose()?
            .unwrap_or(false);

        parse_regex_all(value, numeric_groups, pattern)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ParseRegexAllFn {
    value: Box<dyn Expression>,
    pattern: Regex,
    numeric_groups: Box<dyn Expression>,
}

impl Expression for ParseRegexAllFn {
    fn resolve(&self, ctx: &mut Context) -> Resolved {
        let value = self.value.resolve(ctx)?;
        let numeric_groups = self.numeric_groups.resolve(ctx)?;
        let pattern = &self.pattern;

        parse_regex_all(value, numeric_groups.try_boolean()?, pattern)
    }

    fn type_def(&self, _: (&state::LocalEnv, &state::ExternalEnv)) -> TypeDef {
        TypeDef::array(Collection::from_unknown(
            Kind::object(util::regex_kind(&self.pattern)).or_null(),
        ))
        .fallible()
    }
}

#[cfg(test)]
#[allow(clippy::trivial_regex)]
mod tests {
    use super::*;
    use vector_common::btreemap;

    test_function![
        parse_regex_all => ParseRegexAll;

        matches {
            args: func_args![
                value: "apples and carrots, peaches and peas",
                pattern: Regex::new(r#"(?P<fruit>[\w\.]+) and (?P<veg>[\w]+)"#).unwrap(),
            ],
            want: Ok(value!([{"fruit": "apples",
                              "veg": "carrots"},
                             {"fruit": "peaches",
                              "veg": "peas"}])),
            tdef: TypeDef::array(Collection::from_unknown(Kind::null().or_object(btreemap! {
                    Field::from("fruit") => Kind::bytes(),
                    Field::from("veg") => Kind::bytes(),
                    Field::from("0") => Kind::bytes() | Kind::null(),
                    Field::from("1") => Kind::bytes() | Kind::null(),
                    Field::from("2") => Kind::bytes() | Kind::null(),
                }))).fallible(),
        }

        numeric_groups {
            args: func_args![
                value: "apples and carrots, peaches and peas",
                pattern: Regex::new(r#"(?P<fruit>[\w\.]+) and (?P<veg>[\w]+)"#).unwrap(),
                numeric_groups: true
            ],
            want: Ok(value!([{"fruit": "apples",
                              "veg": "carrots",
                              "0": "apples and carrots",
                              "1": "apples",
                              "2": "carrots"},
                             {"fruit": "peaches",
                              "veg": "peas",
                              "0": "peaches and peas",
                              "1": "peaches",
                              "2": "peas"}])),
            tdef: TypeDef::array(Collection::from_unknown(Kind::null().or_object(btreemap! {
                    Field::from("fruit") => Kind::bytes(),
                    Field::from("veg") => Kind::bytes(),
                    Field::from("0") => Kind::bytes() | Kind::null(),
                    Field::from("1") => Kind::bytes() | Kind::null(),
                    Field::from("2") => Kind::bytes() | Kind::null(),
                }))).fallible(),
        }

        no_matches {
            args: func_args![
                value: "I don't match",
                pattern: Regex::new(r#"(?P<fruit>[\w\.]+) and (?P<veg>[\w]+)"#).unwrap()
            ],
            want: Ok(value!([])),
            tdef: TypeDef::array(Collection::from_unknown(Kind::null().or_object(btreemap! {
                    Field::from("fruit") => Kind::bytes(),
                    Field::from("veg") => Kind::bytes(),
                    Field::from("0") => Kind::bytes() | Kind::null(),
                    Field::from("1") => Kind::bytes() | Kind::null(),
                    Field::from("2") => Kind::bytes() | Kind::null(),
                }))).fallible(),
        }
    ];
}
