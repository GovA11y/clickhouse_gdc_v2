use std::vec;

use super::ast::{
    BinaryOperator, Expr, Function, FunctionArgExpr, Ident, Join, JoinConstraint, JoinOperator,
    ObjectName, OrderByExpr, Query, SelectItem, Statement, TableFactor, TableWithJoins,
    UnaryOperator, Value, WindowSpec,
};
use crate::server::{
    api::query_request::{self, ScalarType},
    Config,
};
use indexmap::IndexMap;
pub mod aliasing;
mod error;
pub use error::QueryBuilderError;

pub enum BoundParam {
    Number(serde_json::Number),
    Value {
        value: serde_json::Value,
        value_type: query_request::ScalarType,
    },
}

fn sql_function(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Function(Function {
        name: ObjectName(vec![Ident::unquoted(name)]),
        args: args.into_iter().map(FunctionArgExpr::Expr).collect(),
        over: None,
        distinct: false,
    })
}

// we use the function name to alias aggregate columns when necessary.
// the name should be reasonable short, and a valid part of a sql identifier when quoted
fn function_name(function: &query_request::SingleColumnAggregateFunction) -> &'static str {
    use query_request::SingleColumnAggregateFunction::*;
    match function {
        Max => "max",
        Min => "min",
        StddevPop => "stddevPop",
        StddevSamp => "stddevSamp",
        Sum => "sum",
        VarPop => "varPop",
        VarSamp => "varSamp",
        Longest => "longest",
        Shortest => "shortest",
    }
}

fn and_reducer(left: Expr, right: Expr) -> Expr {
    Expr::BinaryOp {
        left: Box::new(left),
        op: BinaryOperator::And,
        right: Box::new(right),
    }
}
fn or_reducer(left: Expr, right: Expr) -> Expr {
    Expr::BinaryOp {
        left: Box::new(left),
        op: BinaryOperator::Or,
        right: Box::new(right),
    }
}

fn single_column_aggregate(
    function: &query_request::SingleColumnAggregateFunction,
    column: Expr,
) -> Expr {
    use query_request::SingleColumnAggregateFunction::*;
    match function {
        Max => sql_function("max", vec![column]),
        Min => sql_function("min", vec![column]),
        StddevPop => sql_function("stddevPop", vec![column]),
        StddevSamp => sql_function("stddevSamp", vec![column]),
        Sum => sql_function("sum", vec![column]),
        VarPop => sql_function("varPop", vec![column]),
        VarSamp => sql_function("varSamp", vec![column]),
        Longest => sql_function("max", vec![sql_function("length", vec![column])]),
        Shortest => sql_function("min", vec![sql_function("length", vec![column])]),
    }
}

fn foreach_object_type(query: &query_request::Query) -> String {
    format!(
        "Tuple(rows Array(Tuple(query {})))",
        query_object_type(query)
    )
}

fn query_object_type(query: &query_request::Query) -> String {
    match (&query.fields, &query.aggregates) {
        (None, None) => "Map(Nothing, Nothing)".to_owned(),
        (Some(fields), None) => {
            let fields_type = rows_object_type(fields);
            format!("Tuple(rows Array({}))", fields_type)
        }
        (None, Some(aggregates)) => {
            let aggregates_type = aggregates_object_type(aggregates);
            format!("Tuple(aggregates {})", aggregates_type)
        }
        (Some(fields), Some(aggregates)) => {
            let fields_type = rows_object_type(fields);
            let aggregates_type = aggregates_object_type(aggregates);
            format!(
                "Tuple(rows Array({}), aggregates {})",
                fields_type, aggregates_type
            )
        }
    }
}
fn rows_object_type(fields: &query_request::Fields) -> String {
    if fields.is_empty() {
        "Map(Nothing, Nothing)".to_string()
    } else {
        let field_types = fields
            .iter()
            .map(|(column_name, field)| {
                let field_type = match field {
                    query_request::Field::Column {
                        column: _,
                        column_type,
                    } => type_cast_string(column_type),
                    query_request::Field::Relationship {
                        query,
                        relationship: _,
                    } => query_object_type(query),
                };
                format!("\"{}\" {}", column_name, field_type)
            })
            .collect::<Vec<_>>();
        format!("Tuple({})", field_types.join(", "))
    }
}
fn aggregates_object_type(aggregates: &query_request::Aggregates) -> String {
    if aggregates.is_empty() {
        "Map(Nothing, Nothing)".to_string()
    } else {
        let aggregates_types = aggregates
            .iter()
            .map(|(column_name, aggregate)| {
                let aggregate_type = match aggregate {
                    // note! casting from UInt64 to UInt32 here
                    // UInt64 is serialized as a JSON string, but test suite expects JSON numbers
                    // todo: once we are able to specify return type for these aggregates, update this cast to the correct type
                    query_request::Aggregate::ColumnCount { .. } => "UInt32".to_owned(),
                    query_request::Aggregate::StarCount => "UInt32".to_owned(),
                    query_request::Aggregate::SingleColumn { result_type, .. } => {
                        type_cast_string(result_type)
                    }
                };
                format!("\"{}\" {}", column_name, aggregate_type)
            })
            .collect::<Vec<_>>();
        format!("Tuple({})", aggregates_types.join(", "))
    }
}
/// given a scalar type, return the type for the variant of this type that is nullable
/// used when casting rows to named tuples, which is later used to cast to JSON
/// we always wrap the type name in Nullable() as we don't know if the underlying column is nulable or not
fn type_cast_string(scalar_type: &query_request::ScalarType) -> String {
    use query_request::ScalarType::*;
    match scalar_type {
        Bool => "Nullable(Bool)",
        String => "Nullable(String)",
        FixedString => "Nullable(FixedString)",
        UInt8 => "Nullable(UInt8)",
        UInt16 => "Nullable(UInt16)",
        UInt32 => "Nullable(UInt32)",
        UInt64 => "Nullable(UInt64)",
        UInt128 => "Nullable(UInt128)",
        UInt256 => "Nullable(UInt256)",
        Int8 => "Nullable(Int8)",
        Int16 => "Nullable(Int16)",
        Int32 => "Nullable(Int32)",
        Int64 => "Nullable(Int64)",
        Int128 => "Nullable(Int128)",
        Int256 => "Nullable(Int256)",
        Float32 => "Nullable(Float32)",
        Float64 => "Nullable(Float64)",
        // casting decimal to string. Not sure if this is correct.
        // cannot cast to decimal without making a call on precision and scale
        // could go for max precision, but impossible to know scale
        Decimal => "Nullable(String)",
        Date => "Nullable(Date)",
        Date32 => "Nullable(Date32)",
        DateTime => "Nullable(DateTime)",
        DateTime64 => "Nullable(DateTime64(9))",
        Json => "Nullable(JSON)",
        Uuid => "Nullable(UUID)",
        IPv4 => "Nullable(IPv4)",
        IPv6 => "Nullable(IPv6)",
        Complex => "Nullable(String)",
    }
    .to_owned()
}

pub struct QueryBuilder<'a> {
    request: &'a query_request::QueryRequest,
    bind_params: bool,
    parameters: IndexMap<String, BoundParam>,
    parameter_index: i32,
}

impl<'a> QueryBuilder<'a> {
    fn new(request: &'a query_request::QueryRequest, bind_params: bool) -> Self {
        Self {
            request,
            bind_params,
            parameters: IndexMap::new(),
            parameter_index: 0,
        }
    }
    pub fn build_sql_statement(
        request: &'a query_request::QueryRequest,
        bind_params: bool,
    ) -> Result<Statement, QueryBuilderError> {
        let mut builder = Self::new(request, bind_params);

        let query = builder.root_query()?;

        let statement = Statement(query);

        Ok(statement)
    }

    fn table_relationship(
        &self,
        table: &query_request::TableName,
        relationship_name: &str,
    ) -> Result<&'a query_request::Relationship, QueryBuilderError> {
        let table_relationships = &self.request.table_relationships;
        let source_table = table_relationships
            .iter()
            .find(|table_relationships| table_relationships.source_table == *table)
            .ok_or_else(|| QueryBuilderError::TableMissing(table.to_owned()))?;

        let relationship = source_table
            .relationships
            .get(relationship_name)
            .ok_or_else(|| {
                QueryBuilderError::RelationshipMissingInTable(
                    relationship_name.to_owned(),
                    table.to_owned(),
                )
            })?;

        Ok(relationship)
    }
    fn root_query(&mut self) -> Result<Query, QueryBuilderError> {
        let table = &self.request.table;
        let query = &self.request.query;

        let (root_object_cast_type, root_subquery) = match &self.request.foreach {
            Some(foreach) => {
                // todo: verify that all objects of the foreach collection have the same keys.
                // fail gracefully if not
                // handle the case where there are no objects in the foreach collection. Unsure if this could happen at all?

                let foreach_obj: IndexMap<String, Vec<_>> =
                    foreach
                        .iter()
                        .fold(IndexMap::new(), |mut accumulator, foreach_row| {
                            for (key, value) in foreach_row.iter() {
                                if let Some(foreach_column) = accumulator.get_mut(key) {
                                    foreach_column.push(value.value.to_owned());
                                } else {
                                    accumulator
                                        .insert(key.to_owned(), vec![value.value.to_owned()]);
                                }
                            }
                            accumulator
                        });
                let foreach_obj_json_string = serde_json::to_string(&foreach_obj)
                    .map_err(|err| QueryBuilderError::Internal(err.to_string()))?;

                let foreach_expr = Function {
                    name: ObjectName(vec![Ident::unquoted("format")]),
                    args: vec![
                        FunctionArgExpr::Expr(Expr::Identifier(Ident::unquoted("JSONColumns"))),
                        FunctionArgExpr::Expr(Expr::Value(Value::SingleQuotedString(
                            foreach_obj_json_string,
                        ))),
                    ],
                    over: None,
                    distinct: false,
                };

                let foreach_table = TableFactor::TableFunction {
                    function: foreach_expr,
                    alias: Some(Ident::quoted("_foreach")),
                };
                let foreach_columns: Vec<_> = foreach[0].keys().collect();

                (
                    foreach_object_type(query),
                    self.query_subquery(
                        table,
                        &vec![],
                        query,
                        Some((foreach_table, &foreach_columns)),
                    )?,
                )
            }
            None => (
                query_object_type(query),
                self.query_subquery(table, &vec![], query, None)?,
            ),
        };

        let query_expr =
            Expr::CompoundIdentifier(vec![Ident::quoted("_query"), Ident::quoted("query")]);

        let root_projection = vec![SelectItem::ExprWithAlias {
            expr: sql_function(
                "toJSONString",
                vec![sql_function(
                    "cast",
                    vec![
                        query_expr,
                        Expr::Value(Value::SingleQuotedString(root_object_cast_type)),
                    ],
                )],
            ),
            alias: Ident::quoted("query"),
        }];

        let root_from = vec![TableWithJoins {
            relation: TableFactor::Derived {
                subquery: root_subquery,
                alias: Some(Ident::quoted("_query")),
            },
            joins: vec![],
        }];

        Ok(Query::new().projection(root_projection).from(root_from))
    }
    fn query_subquery(
        &mut self,
        table: &query_request::TableName,
        join_cols: &Vec<&String>,
        query: &query_request::Query,
        foreach: Option<(TableFactor, &[&String])>,
    ) -> Result<Box<Query>, QueryBuilderError> {
        let foreach_columns = foreach
            .as_ref()
            .map(|(_, foreach_columns)| *foreach_columns);
        let (rows_subquery, rows_expr) = match &query.fields {
            None => (None, None),
            Some(fields) => {
                let rows_subquery =
                    self.rows_subquery(table, join_cols, fields, query, &foreach_columns)?;
                let rows_expr =
                    Expr::CompoundIdentifier(vec![Ident::quoted("_rows"), Ident::quoted("rows")]);
                (Some(rows_subquery), Some(rows_expr))
            }
        };
        let (aggregates_subquery, aggregates_expr) = match &query.aggregates {
            None => (None, None),
            Some(aggregates) => {
                let aggregates_subquery = self.aggregates_subquery(
                    table,
                    join_cols,
                    aggregates,
                    query,
                    &foreach_columns,
                )?;
                let aggregates_expr = Expr::CompoundIdentifier(vec![
                    Ident::quoted("_aggregates"),
                    Ident::quoted("aggregates"),
                ]);
                (Some(aggregates_subquery), Some(aggregates_expr))
            }
        };

        let query_expr = match (rows_expr, aggregates_expr) {
            (None, None) => sql_function("map", vec![]),
            (None, Some(aggregates_expr)) => sql_function("tuple", vec![aggregates_expr]),
            (Some(rows_expr), None) => sql_function("tuple", vec![rows_expr]),
            (Some(rows_expr), Some(aggregates_expr)) => {
                sql_function("tuple", vec![rows_expr, aggregates_expr])
            }
        };

        let base_expr = if foreach.is_some() {
            sql_function(
                "tuple",
                vec![sql_function(
                    "groupArray",
                    vec![sql_function("tuple", vec![query_expr])],
                )],
            )
        } else {
            query_expr
        };

        let base_select_item = SelectItem::ExprWithAlias {
            expr: base_expr,
            alias: Ident::quoted("query"),
        };

        let query_projection = vec![base_select_item]
            .into_iter()
            .chain(join_cols.iter().map(|col| SelectItem::ExprWithAlias {
                expr: Expr::CompoundIdentifier(vec![Ident::quoted(format!("_selection.{col}"))]),
                alias: Ident::quoted(format!("_selection.{col}")),
            }))
            .collect();

        // note: if rows not required. join not required either
        // also note: will need to change this cross join for subqueries that do have some kind of predicate
        let query_from = match foreach {
            Some((foreach_table, foreach_columns)) => {
                let rows_join = rows_subquery.map(|rows_subquery| {
                    let join_expr = foreach_columns
                        .iter()
                        .map(|&col| {
                            let left = Expr::CompoundIdentifier(vec![
                                Ident::quoted("_foreach"),
                                Ident::quoted(col),
                            ]);
                            let right = Expr::CompoundIdentifier(vec![
                                Ident::quoted("_rows"),
                                Ident::quoted(format!("_foreach.{}", col)),
                            ]);
                            Expr::BinaryOp {
                                left: Box::new(left),
                                op: BinaryOperator::Eq,
                                right: Box::new(right),
                            }
                        })
                        .reduce(and_reducer)
                        .unwrap_or(Expr::Value(Value::Boolean(true)));
                    Join {
                        relation: TableFactor::Derived {
                            subquery: rows_subquery,
                            alias: Some(Ident::quoted("_rows")),
                        },
                        join_operator: JoinOperator::LeftOuter(JoinConstraint::On(join_expr)),
                    }
                });
                let aggregates_join = aggregates_subquery.map(|aggregates_subquery| {
                    let join_expr = foreach_columns
                        .iter()
                        .map(|&col| {
                            let left = Expr::CompoundIdentifier(vec![
                                Ident::quoted("_foreach"),
                                Ident::quoted(col),
                            ]);
                            let right = Expr::CompoundIdentifier(vec![
                                Ident::quoted("_aggregates"),
                                Ident::quoted(format!("_foreach.{}", col)),
                            ]);
                            Expr::BinaryOp {
                                left: Box::new(left),
                                op: BinaryOperator::Eq,
                                right: Box::new(right),
                            }
                        })
                        .reduce(and_reducer)
                        .unwrap_or(Expr::Value(Value::Boolean(true)));
                    Join {
                        relation: TableFactor::Derived {
                            subquery: aggregates_subquery,
                            alias: Some(Ident::quoted("_aggregates")),
                        },
                        join_operator: JoinOperator::LeftOuter(JoinConstraint::On(join_expr)),
                    }
                });

                let joins = match (rows_join, aggregates_join) {
                    (None, None) => vec![],
                    (None, Some(aggregates_join)) => vec![aggregates_join],
                    (Some(rows_join), None) => vec![rows_join],
                    (Some(rows_join), Some(aggregates_join)) => vec![rows_join, aggregates_join],
                };

                vec![TableWithJoins {
                    relation: foreach_table,
                    joins,
                }]
            }
            None => match (rows_subquery, aggregates_subquery) {
                (None, None) => vec![],
                (None, Some(aggregates_subquery)) => vec![TableWithJoins {
                    relation: TableFactor::Derived {
                        subquery: aggregates_subquery,
                        alias: Some(Ident::quoted("_aggregates")),
                    },
                    joins: vec![],
                }],
                (Some(rows_subquery), None) => vec![TableWithJoins {
                    relation: TableFactor::Derived {
                        subquery: rows_subquery,
                        alias: Some(Ident::quoted("_rows")),
                    },
                    joins: vec![],
                }],
                (Some(rows_subquery), Some(aggregates_subquery)) => vec![TableWithJoins {
                    relation: TableFactor::Derived {
                        subquery: rows_subquery,
                        alias: Some(Ident::quoted("_rows")),
                    },
                    joins: vec![Join {
                        relation: TableFactor::Derived {
                            subquery: aggregates_subquery,
                            alias: Some(Ident::quoted("_aggregates")),
                        },
                        join_operator: if join_cols.is_empty() {
                            JoinOperator::CrossJoin
                        } else {
                            let cols = join_cols
                                .iter()
                                .map(|col| Ident::quoted(format!("_selection.{col}")))
                                .collect();
                            JoinOperator::FullOuter(JoinConstraint::Using(cols))
                        },
                    }],
                }],
            },
        };

        Ok(Query::new()
            .projection(query_projection)
            .from(query_from)
            .boxed())
    }
    fn rows_subquery(
        &mut self,
        table: &query_request::TableName,
        join_cols: &[&String],
        fields: &query_request::Fields,
        query: &query_request::Query,
        foreach_columns: &Option<&[&String]>,
    ) -> Result<Box<Query>, QueryBuilderError> {
        let row_subquery = self.row_subquery(table, join_cols, fields, query, foreach_columns)?;

        let column_exprs = fields
            .iter()
            .map(|(alias, _)| {
                (
                    alias.clone(),
                    Expr::CompoundIdentifier(vec![
                        Ident::quoted("_row"),
                        Ident::quoted(format!("_projection.{alias}")),
                    ]),
                )
            })
            .collect::<Vec<_>>();

        let rows_projection = join_cols
            .iter()
            .map(|col| SelectItem::ExprWithAlias {
                expr: Expr::CompoundIdentifier(vec![
                    Ident::quoted("_row"),
                    Ident::quoted(format!("_selection.{col}")),
                ]),
                alias: Ident::quoted(format!("_selection.{col}")),
            })
            .chain(vec![SelectItem::ExprWithAlias {
                expr: if column_exprs.is_empty() {
                    sql_function("groupArray", vec![sql_function("map", vec![])])
                } else {
                    sql_function(
                        "groupArray",
                        vec![sql_function(
                            "tuple",
                            column_exprs.into_iter().map(|(_, expr)| expr).collect(),
                        )],
                    )
                },
                alias: Ident::quoted("rows"),
            }]);

        let rows_projection = if let Some(foreach_columns) = foreach_columns {
            rows_projection
                .chain(foreach_columns.iter().map(|col| {
                    SelectItem::UnnamedExpr(Expr::CompoundIdentifier(vec![
                        Ident::quoted("_row"),
                        Ident::quoted(format!("_foreach.{col}")),
                    ]))
                }))
                .collect()
        } else {
            rows_projection.collect()
        };

        let rows_from = vec![TableWithJoins {
            relation: TableFactor::Derived {
                subquery: row_subquery,
                alias: Some(Ident::quoted("_row")),
            },
            joins: vec![],
        }];

        let rows_selection = self.limit_offset_expression(&query.limit, &query.offset);

        let rows_group_by = join_cols.iter().map(|&col| {
            Expr::CompoundIdentifier(vec![
                Ident::quoted("_row"),
                Ident::quoted(format!("_selection.{col}")),
            ])
        });

        let rows_group_by = if let Some(foreach_columns) = foreach_columns {
            rows_group_by
                .chain(foreach_columns.iter().map(|col| {
                    Expr::CompoundIdentifier(vec![
                        Ident::quoted("_row"),
                        Ident::quoted(format!("_foreach.{col}")),
                    ])
                }))
                .collect()
        } else {
            rows_group_by.collect()
        };

        Ok(Query::new()
            .projection(rows_projection)
            .from(rows_from)
            .predicate(rows_selection)
            .group_by(rows_group_by)
            .boxed())
    }
    fn row_subquery(
        &mut self,
        table: &query_request::TableName,
        join_cols: &[&String],
        fields: &query_request::Fields,
        query: &query_request::Query,
        foreach_columns: &Option<&[&String]>,
    ) -> Result<Box<Query>, QueryBuilderError> {
        let selection_columns_expressions =
            join_cols.iter().map(|&col| SelectItem::ExprWithAlias {
                expr: Expr::CompoundIdentifier(vec![Ident::quoted("_origin"), Ident::quoted(col)]),
                alias: Ident::quoted(format!("_selection.{col}")),
            });

        let row_columns_expressions = fields.iter().map(|(alias, field)| match field {
            query_request::Field::Column {
                column,
                column_type,
            } => {
                let identifier =
                    Expr::CompoundIdentifier(vec![Ident::quoted("_origin"), Ident::quoted(column)]);

                let expr = match column_type {
                    ScalarType::Complex => sql_function("toJSONString", vec![identifier]),
                    _ => identifier,
                };
                SelectItem::ExprWithAlias {
                    expr,
                    alias: Ident::quoted(format!("_projection.{alias}")),
                }
            }
            query_request::Field::Relationship { .. } => SelectItem::ExprWithAlias {
                expr: Expr::CompoundIdentifier(vec![
                    Ident::quoted(format!("_rel.{alias}")),
                    Ident::quoted("query"),
                ]),
                alias: Ident::quoted(format!("_projection.{alias}")),
            },
        });

        let row_foreach_column_expressions = match foreach_columns {
            Some(foreach_columns) => foreach_columns
                .iter()
                .map(|&col| SelectItem::ExprWithAlias {
                    expr: Expr::CompoundIdentifier(vec![
                        Ident::quoted("_origin"),
                        Ident::quoted(col),
                    ]),
                    alias: Ident::quoted(format!("_foreach.{col}")),
                })
                .collect(),
            None => vec![],
        };

        let (order_by, order_by_joins) = self.order_by_expressions_joins(table, &query.order_by)?;

        let partition_cols = match foreach_columns {
            Some(foreach_columns) => join_cols.iter().chain(*foreach_columns).copied().collect(),
            None => join_cols.to_vec(),
        };

        let row_number_expression = SelectItem::ExprWithAlias {
            expr: self.row_number_expression(&partition_cols, order_by),
            alias: Ident::quoted("_rn"),
        };

        let row_projection = selection_columns_expressions
            .chain(row_columns_expressions)
            .chain(row_foreach_column_expressions)
            .chain([row_number_expression])
            .collect();

        let (row_selection, exists_joins) = match &query.selection {
            Some(expression) => {
                let mut exists_index = 0;
                let (expr, joins) = self.selection_expression(
                    expression,
                    &mut exists_index,
                    true,
                    "_origin",
                    table,
                )?;
                (Some(expr), joins)
            }
            None => (None, vec![]),
        };

        let relationship_joins = fields
            .iter()
            .filter_map(|(alias, field)| match field {
                query_request::Field::Column { .. } => None,
                query_request::Field::Relationship {
                    query,
                    relationship,
                } => Some((alias, query, relationship)),
            })
            .map(|(alias, query, relationship)| {
                {
                    // todo: handle case where the relationship info is missing gracefully
                    let relationship = self.table_relationship(table, relationship)?;

                    let join_expr = relationship
                        .column_mapping
                        .iter()
                        .map(|(source_col, target_col)| Expr::BinaryOp {
                            left: Box::new(Expr::CompoundIdentifier(vec![
                                Ident::quoted("_origin"),
                                Ident::quoted(source_col),
                            ])),
                            op: BinaryOperator::Eq,
                            right: Box::new(Expr::CompoundIdentifier(vec![
                                Ident::quoted(format!("_rel.{alias}")),
                                Ident::quoted(format!("_selection.{target_col}")),
                            ])),
                        })
                        .reduce(and_reducer)
                        .unwrap_or(Expr::Value(Value::Boolean(true)));

                    let table = &relationship.target_table;
                    let join_cols = &relationship.column_mapping.values().collect();

                    Ok(Join {
                        relation: TableFactor::Derived {
                            subquery: self.query_subquery(table, join_cols, query, None)?,
                            alias: Some(Ident::quoted(format!("_rel.{alias}"))),
                        },
                        join_operator: JoinOperator::LeftOuter(JoinConstraint::On(join_expr)),
                    })
                }
            })
            .collect::<Result<Vec<_>, _>>()?;

        let row_from = vec![TableWithJoins {
            relation: TableFactor::Table {
                name: ObjectName(table.iter().map(Ident::quoted).collect()),
                alias: Some(Ident::quoted("_origin")),
            },
            joins: relationship_joins
                .into_iter()
                .chain(order_by_joins)
                .chain(exists_joins)
                .collect(),
        }];

        let row_order_by = vec![OrderByExpr {
            asc: None,
            expr: Expr::CompoundIdentifier(vec![Ident::quoted("_rn")]),
            nulls_first: None,
        }];

        Ok(Query::new()
            .projection(row_projection)
            .from(row_from)
            .predicate(row_selection)
            .order_by(row_order_by)
            .boxed())
    }
    fn aggregates_subquery(
        &mut self,
        table: &query_request::TableName,
        join_cols: &[&String],
        aggregates: &query_request::Aggregates,
        query: &query_request::Query,
        foreach_columns: &Option<&[&String]>,
    ) -> Result<Box<Query>, QueryBuilderError> {
        let aggregate_subquery =
            self.aggregate_subquery(table, join_cols, aggregates, query, foreach_columns)?;
        let column_exprs = aggregates
            .iter()
            .map(|(alias, field)| {
                let colum_expr = match field {
                    query_request::Aggregate::StarCount => Expr::Function(Function {
                        name: ObjectName(vec![Ident::unquoted("COUNT")]),
                        args: vec![FunctionArgExpr::Wildcard],
                        over: None,
                        distinct: false,
                    }),
                    query_request::Aggregate::ColumnCount {
                        column: _,
                        distinct,
                    } => {
                        let column = Expr::CompoundIdentifier(vec![
                            Ident::quoted("_row"),
                            Ident::quoted(format!("_projection.{alias}")),
                        ]);
                        Expr::Function(Function {
                            name: ObjectName(vec![Ident::unquoted("COUNT")]),
                            args: vec![FunctionArgExpr::Expr(column)],
                            over: None,
                            distinct: distinct.to_owned(),
                        })
                    }
                    query_request::Aggregate::SingleColumn { function, .. } => {
                        let column = Expr::CompoundIdentifier(vec![
                            Ident::quoted("_row"),
                            Ident::quoted(format!("_projection.{alias}")),
                        ]);
                        single_column_aggregate(function, column)
                    }
                };

                (alias.clone(), colum_expr)
            })
            .collect::<Vec<_>>();

        let aggregates_projection = join_cols
            .iter()
            .map(|col| SelectItem::ExprWithAlias {
                expr: Expr::CompoundIdentifier(vec![
                    Ident::quoted("_row"),
                    Ident::quoted(format!("_selection.{col}")),
                ]),
                alias: Ident::quoted(format!("_selection.{col}")),
            })
            .chain(vec![SelectItem::ExprWithAlias {
                expr: if column_exprs.is_empty() {
                    sql_function("map", vec![])
                } else {
                    sql_function(
                        "tuple",
                        column_exprs.into_iter().map(|(_, expr)| expr).collect(),
                    )
                },
                alias: Ident::quoted("aggregates"),
            }]);

        let aggregates_projection = if let Some(foreach_columns) = foreach_columns {
            aggregates_projection
                .chain(foreach_columns.iter().map(|col| {
                    SelectItem::UnnamedExpr(Expr::CompoundIdentifier(vec![
                        Ident::quoted("_row"),
                        Ident::quoted(format!("_foreach.{col}")),
                    ]))
                }))
                .collect()
        } else {
            aggregates_projection.collect()
        };

        let aggregates_from = vec![TableWithJoins {
            relation: TableFactor::Derived {
                subquery: aggregate_subquery,
                alias: Some(Ident::quoted("_row")),
            },
            joins: vec![],
        }];

        // todo: apply limit/offset here using where clause and row number
        let aggregates_selection =
            self.limit_offset_expression(&query.aggregates_limit, &query.offset);

        let aggregates_group_by = join_cols.iter().map(|&col| {
            Expr::CompoundIdentifier(vec![
                Ident::quoted("_row"),
                Ident::quoted(format!("_selection.{col}")),
            ])
        });

        let aggregates_group_by = if let Some(foreach_columns) = foreach_columns {
            aggregates_group_by
                .chain(foreach_columns.iter().map(|col| {
                    Expr::CompoundIdentifier(vec![
                        Ident::quoted("_row"),
                        Ident::quoted(format!("_foreach.{col}")),
                    ])
                }))
                .collect()
        } else {
            aggregates_group_by.collect()
        };

        Ok(Query::new()
            .projection(aggregates_projection)
            .from(aggregates_from)
            .predicate(aggregates_selection)
            .group_by(aggregates_group_by)
            .boxed())
    }
    fn aggregate_subquery(
        &mut self,
        table: &query_request::TableName,
        join_cols: &[&String],
        aggregates: &query_request::Aggregates,
        query: &query_request::Query,
        foreach_columns: &Option<&[&String]>,
    ) -> Result<Box<Query>, QueryBuilderError> {
        // todo: add columns needed for joinning to parent table here, if needed
        let selection_columns_expressions =
            join_cols.iter().map(|&col| SelectItem::ExprWithAlias {
                expr: Expr::CompoundIdentifier(vec![Ident::quoted("_origin"), Ident::quoted(col)]),
                alias: Ident::quoted(format!("_selection.{col}")),
            });

        let aggregate_columns_expressions =
            aggregates.iter().filter_map(|(alias, agg)| match agg {
                query_request::Aggregate::ColumnCount { column, .. }
                | query_request::Aggregate::SingleColumn { column, .. } => {
                    Some(SelectItem::ExprWithAlias {
                        expr: Expr::CompoundIdentifier(vec![
                            Ident::quoted("_origin"),
                            Ident::quoted(column),
                        ]),
                        alias: Ident::quoted(format!("_projection.{alias}")),
                    })
                }
                query_request::Aggregate::StarCount => None,
            });

        let aggregate_foreach_column_expressions = match foreach_columns {
            Some(foreach_columns) => foreach_columns
                .iter()
                .map(|&col| SelectItem::ExprWithAlias {
                    expr: Expr::CompoundIdentifier(vec![
                        Ident::quoted("_origin"),
                        Ident::quoted(col),
                    ]),
                    alias: Ident::quoted(format!("_foreach.{col}")),
                })
                .collect(),
            None => vec![],
        };

        let (order_by, order_by_joins) = self.order_by_expressions_joins(table, &query.order_by)?;

        let partition_cols = match foreach_columns {
            Some(foreach_columns) => join_cols.iter().chain(*foreach_columns).copied().collect(),
            None => join_cols.to_vec(),
        };

        let row_number_expression = SelectItem::ExprWithAlias {
            expr: self.row_number_expression(&partition_cols, order_by),
            alias: Ident::quoted("_rn"),
        };

        let aggregate_projection = selection_columns_expressions
            .chain(aggregate_columns_expressions)
            .chain(aggregate_foreach_column_expressions)
            .chain(vec![row_number_expression])
            .collect();

        let (aggregate_selection, exists_joins) = match &query.selection {
            Some(expression) => {
                let mut exists_index = 0;
                let (expr, joins) = self.selection_expression(
                    expression,
                    &mut exists_index,
                    true,
                    "_origin",
                    table,
                )?;
                (Some(expr), joins)
            }
            None => (None, vec![]),
        };

        let aggregate_from = vec![TableWithJoins {
            relation: TableFactor::Table {
                name: ObjectName(table.iter().map(|s| Ident::quoted(s)).collect()),
                alias: Some(Ident::quoted("_origin")),
            },
            joins: exists_joins.into_iter().chain(order_by_joins).collect(),
        }];

        Ok(Query::new()
            .projection(aggregate_projection)
            .from(aggregate_from)
            .predicate(aggregate_selection)
            .boxed())
    }
    fn order_by_expressions_joins(
        &mut self,
        table: &query_request::TableName,
        order_by: &Option<query_request::OrderBy>,
    ) -> Result<(Vec<OrderByExpr>, Vec<Join>), QueryBuilderError> {
        match order_by {
            None => Ok((vec![], vec![])),
            Some(order_by) => {
                // discard parent columns at the root level, since all columns are exposed on origin
                let (_, order_by_joins) =
                    self.order_by_joins(table, &vec![], &order_by.relations, order_by)?;

                let order_by = order_by
                    .elements
                    .iter()
                    .map(|element| {
                        let table_alias = if element.target_path.is_empty() {
                            "_origin".to_string()
                        } else {
                            format!("_ord.{}", element.target_path.join("."))
                        };
                        let column_alias = match &element.target {
                            query_request::OrderByTarget::StarCountAggregate => {
                                "_count".to_string()
                            }
                            query_request::OrderByTarget::SingleColumnAggregate {
                                column,
                                function,
                                result_type: _,
                            } => {
                                format!("_agg.{}.{}", function_name(function), column)
                            }

                            query_request::OrderByTarget::Column { column } => {
                                if element.target_path.is_empty() {
                                    column.to_owned()
                                } else {
                                    format!("_col.{column}")
                                }
                            }
                        };

                        self.order_by_expr(&table_alias, &column_alias, element)
                    })
                    .collect();

                Ok((order_by, order_by_joins))
            }
        }
    }
    fn order_by_expr(
        &mut self,
        table_alias: &str,
        column_alias: &str,
        order_by_element: &query_request::OrderByElement,
    ) -> OrderByExpr {
        let column = Expr::CompoundIdentifier(vec![
            Ident::quoted(table_alias),
            Ident::quoted(column_alias),
        ]);
        let expr = match &order_by_element.target {
            // default to sorting on 0 for count(*)
            query_request::OrderByTarget::StarCountAggregate => sql_function(
                "COALESCE",
                vec![column, Expr::Value(Value::Number("0".to_owned()))],
            ),
            // sort on default value for aggregates
            query_request::OrderByTarget::SingleColumnAggregate { result_type, .. } => {
                use query_request::ScalarType::*;
                let default_sorting_value = match result_type {
                    Bool => Value::Null,
                    String | FixedString => Value::SingleQuotedString("".to_owned()),
                    UInt8 | UInt16 | UInt32 | UInt64 | UInt128 | UInt256 | Int8 | Int16 | Int32
                    | Int64 | Int128 | Int256 | Float32 | Float64 | Decimal => {
                        Value::Number("0".to_owned())
                    }
                    Date | Date32 | DateTime | DateTime64 => Value::Null,
                    Json => Value::Null,
                    Uuid => Value::Null,
                    IPv4 | IPv6 => Value::Null,
                    Complex => Value::Null,
                };
                sql_function("COALESCE", vec![column, Expr::Value(default_sorting_value)])
            }
            query_request::OrderByTarget::Column { .. } => column,
        };
        OrderByExpr {
            expr,
            asc: Some(match order_by_element.order_direction {
                query_request::OrderDirection::Asc => true,
                query_request::OrderDirection::Desc => false,
            }),
            nulls_first: Some(match order_by_element.order_direction {
                query_request::OrderDirection::Asc => false,
                query_request::OrderDirection::Desc => true,
            }),
        }
    }
    fn order_by_joins(
        &mut self,
        table: &query_request::TableName,
        source_path: &Vec<String>,
        relations: &IndexMap<String, query_request::OrderByRelation>,
        order_by: &query_request::OrderBy,
    ) -> Result<(Vec<String>, Vec<Join>), QueryBuilderError> {
        let mut joins = vec![];
        let mut parent_join_columns = vec![];
        let parent_alias = if source_path.is_empty() {
            "_origin".to_string()
        } else {
            format!("_ord.{}", source_path.join("."))
        };
        for (relationship_name, order_by_relation) in relations {
            let relationship = self.table_relationship(table, relationship_name)?;

            // parent table will need to expose these columns for this table to join on
            for column in relationship.column_mapping.keys() {
                if !parent_join_columns.contains(column) {
                    parent_join_columns.push(column.clone());
                }
            }

            let child_path = [&source_path[..], &[relationship_name.to_owned()]].concat();
            let child_alias = format!("_ord.{}", child_path.join("."));

            // child columns will be used by subsequent joins to join to this table
            let (child_columns, child_joins) = self.order_by_joins(
                &relationship.target_table,
                &child_path,
                &order_by_relation.subrelations,
                order_by,
            )?;

            let mut projection_cols = IndexMap::new();
            let mut group_by_cols = IndexMap::new();

            for element in &order_by.elements {
                if element.target_path == child_path {
                    // add the column to the projection
                    let col_alias = match &element.target {
                        query_request::OrderByTarget::StarCountAggregate => "_count".to_string(),
                        query_request::OrderByTarget::SingleColumnAggregate {
                            column,
                            function,
                            result_type: _,
                        } => format!("_agg.{}.{}", function_name(function), column),
                        query_request::OrderByTarget::Column { column } => {
                            format!("_col.{column}")
                        }
                    };
                    let projection_expr = match &element.target {
                        query_request::OrderByTarget::StarCountAggregate => {
                            Expr::Function(Function {
                                name: ObjectName(vec![Ident::unquoted("COUNT")]),
                                args: vec![FunctionArgExpr::Wildcard],
                                over: None,
                                distinct: false,
                            })
                        }
                        query_request::OrderByTarget::SingleColumnAggregate {
                            column,
                            function,
                            result_type: _,
                        } => {
                            let column_expr = Expr::Identifier(Ident::quoted(column));
                            single_column_aggregate(function, column_expr)
                        }
                        query_request::OrderByTarget::Column { column } => {
                            Expr::Identifier(Ident::quoted(column))
                        }
                    };
                    let projection_col = SelectItem::ExprWithAlias {
                        expr: projection_expr,
                        alias: Ident::quoted(&col_alias),
                    };
                    projection_cols.insert(col_alias, projection_col);
                    // add the column to the group by clause, if it's not an aggregate
                    if let query_request::OrderByTarget::Column { column } = &element.target {
                        let group_by_col = Expr::Identifier(Ident::quoted(column));
                        group_by_cols.insert(column, group_by_col);
                    }
                }
            }

            // add columns needed joining to the parent table to the projection and group by, if not duplicates
            for column in relationship.column_mapping.values() {
                let col_alias = format!("_col.{column}");
                if !projection_cols.contains_key(&col_alias) {
                    let projection_col = SelectItem::ExprWithAlias {
                        expr: Expr::Identifier(Ident::quoted(column)),
                        alias: Ident::quoted(col_alias.clone()),
                    };
                    projection_cols.insert(col_alias, projection_col);
                }
                if !group_by_cols.contains_key(column) {
                    let group_by_col = Expr::Identifier(Ident::quoted(column));
                    group_by_cols.insert(column, group_by_col);
                }
            }

            for column in &child_columns {
                let col_alias = format!("_col.{column}");
                if !projection_cols.contains_key(&col_alias) {
                    let projection_col = SelectItem::ExprWithAlias {
                        expr: Expr::Identifier(Ident::quoted(column)),
                        alias: Ident::quoted(col_alias.clone()),
                    };
                    projection_cols.insert(col_alias, projection_col);
                }
                if !group_by_cols.contains_key(column) {
                    let group_by_col = Expr::Identifier(Ident::quoted(column));
                    group_by_cols.insert(column, group_by_col);
                }
            }

            let (join_selection, exists_joins) = match &order_by_relation.selection {
                Some(expression) => {
                    let mut exists_index = 0;
                    let (expr, joins) = self.selection_expression(
                        expression,
                        &mut exists_index,
                        true,
                        "_origin",
                        table,
                    )?;
                    (Some(expr), joins)
                }
                None => (None, vec![]),
            };

            // cols for join and ordering, aggregates
            let join_projection = projection_cols.into_values().collect();
            let join_from = vec![TableWithJoins {
                relation: TableFactor::Table {
                    name: ObjectName(
                        relationship
                            .target_table
                            .iter()
                            .map(|s| Ident::quoted(s))
                            .collect(),
                    ),
                    alias: Some(Ident::quoted("_origin")),
                },
                joins: exists_joins,
            }];
            let join_group_by = group_by_cols.into_values().collect();

            let join_subquery = Query::new()
                .projection(join_projection)
                .from(join_from)
                .predicate(join_selection)
                .group_by(join_group_by)
                .boxed();

            let join = Join {
                relation: TableFactor::Derived {
                    subquery: join_subquery,
                    alias: Some(Ident::quoted(&child_alias)),
                },
                join_operator: JoinOperator::LeftOuter(JoinConstraint::On(
                    relationship
                        .column_mapping
                        .iter()
                        .map(|(source_col, target_col)| Expr::BinaryOp {
                            left: Box::new(Expr::CompoundIdentifier(vec![
                                Ident::quoted(parent_alias.clone()),
                                Ident::quoted(if source_path.is_empty() {
                                    source_col.clone()
                                } else {
                                    format!("_col.{source_col}")
                                }),
                            ])),
                            op: BinaryOperator::Eq,
                            right: Box::new(Expr::CompoundIdentifier(vec![
                                Ident::quoted(child_alias.clone()),
                                Ident::quoted(format!("_col.{target_col}")),
                            ])),
                        })
                        .reduce(and_reducer)
                        .unwrap_or(Expr::Value(Value::Boolean(true))),
                )),
            };

            joins.push(join);
            joins.extend(child_joins);
        }
        Ok((parent_join_columns, joins))
    }
    fn row_number_expression(
        &mut self,
        partition_by: &[&String],
        order_by: Vec<OrderByExpr>,
    ) -> Expr {
        // todo: partition by any columns we used to join the relationship.
        let partition_by = partition_by
            .iter()
            .map(|&col| {
                Expr::CompoundIdentifier(vec![Ident::quoted("_origin"), Ident::quoted(col)])
            })
            .collect();
        Expr::Function(Function {
            name: ObjectName(vec![Ident::unquoted("row_number")]),
            args: vec![],
            over: Some(WindowSpec {
                partition_by,
                order_by,
            }),
            distinct: false,
        })
    }
    fn limit_offset_expression(
        &mut self,
        limit: &Option<serde_json::Number>,
        offset: &Option<serde_json::Number>,
    ) -> Option<Expr> {
        match (limit, offset) {
            (None, None) => None,
            (None, Some(offset)) => Some(Expr::BinaryOp {
                left: Box::new(Expr::CompoundIdentifier(vec![
                    Ident::quoted("_row"),
                    Ident::quoted("_rn"),
                ])),
                op: BinaryOperator::Gt,
                right: Box::new(Expr::Value(Value::Number(offset.to_string()))),
            }),
            (Some(limit), None) => Some(Expr::BinaryOp {
                left: Box::new(Expr::CompoundIdentifier(vec![
                    Ident::quoted("_row"),
                    Ident::quoted("_rn"),
                ])),
                op: BinaryOperator::LtEq,
                right: Box::new(Expr::Value(Value::Number(limit.to_string()))),
            }),
            (Some(limit), Some(offset)) => Some(Expr::BinaryOp {
                left: Box::new(Expr::BinaryOp {
                    left: Box::new(Expr::CompoundIdentifier(vec![
                        Ident::quoted("_row"),
                        Ident::quoted("_rn"),
                    ])),
                    op: BinaryOperator::Gt,
                    right: Box::new(Expr::Value(Value::Number(offset.to_string()))),
                }),
                op: BinaryOperator::And,
                right: Box::new(Expr::BinaryOp {
                    left: Box::new(Expr::CompoundIdentifier(vec![
                        Ident::quoted("_row"),
                        Ident::quoted("_rn"),
                    ])),
                    op: BinaryOperator::LtEq,
                    right: Box::new(Expr::Value(Value::Number(
                        // todo: this is probably safe, but could also not be.
                        // handle failure gracefully, by returning an error
                        (limit.as_u64().unwrap() + offset.as_u64().unwrap()).to_string(),
                    ))),
                }),
            }),
        }
    }
    fn selection_expression(
        &mut self,
        expression: &query_request::Expression,
        exists_index: &mut usize,
        origin: bool,
        table_alias: &str,
        table: &query_request::TableName,
    ) -> Result<(Expr, Vec<Join>), QueryBuilderError> {
        match expression {
            query_request::Expression::And { expressions } => {
                let exprs = expressions
                    .iter()
                    .map(|expression| {
                        self.selection_expression(
                            expression,
                            exists_index,
                            origin,
                            table_alias,
                            table,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;

                let and_expr = exprs
                    .into_iter()
                    .reduce(|left, right| {
                        (
                            and_reducer(left.0, right.0),
                            left.1.into_iter().chain(right.1).collect(),
                        )
                    })
                    .map(|(expr, joins)| match expr {
                        Expr::BinaryOp {
                            op: BinaryOperator::And,
                            ..
                        } => (Expr::Nested(Box::new(expr)), joins),
                        _ => (expr, joins),
                    })
                    .unwrap_or_else(|| (Expr::Value(Value::Boolean(true)), vec![]));

                Ok(and_expr)
            }
            query_request::Expression::Or { expressions } => {
                let exprs = expressions
                    .iter()
                    .map(|expression| {
                        self.selection_expression(
                            expression,
                            exists_index,
                            origin,
                            table_alias,
                            table,
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;

                let or_expr = exprs
                    .into_iter()
                    .reduce(|left, right| {
                        (
                            or_reducer(left.0, right.0),
                            left.1.into_iter().chain(right.1).collect(),
                        )
                    })
                    .map(|(expr, joins)| match expr {
                        Expr::BinaryOp {
                            op: BinaryOperator::Or,
                            ..
                        } => (Expr::Nested(Box::new(expr)), joins),
                        _ => (expr, joins),
                    })
                    .unwrap_or_else(|| (Expr::Value(Value::Boolean(false)), vec![]));

                Ok(or_expr)
            }

            query_request::Expression::Not { expression } => {
                let (expr, joins) = self.selection_expression(
                    expression,
                    exists_index,
                    origin,
                    table_alias,
                    table,
                )?;
                let expr = Expr::UnaryOp {
                    op: UnaryOperator::Not,
                    expr: Box::new(expr),
                };
                Ok((expr, joins))
            }
            query_request::Expression::UnaryComparisonOperator { column, operator } => {
                let expr = Box::new(self.comparison_column(table_alias, column)?);
                let expr = match operator {
                    query_request::UnaryComparisonOperator::IsNull => Expr::IsNull(expr),
                };
                Ok((expr, vec![]))
            }
            query_request::Expression::BinaryComparisonOperator {
                column,
                operator,
                value,
            } => {
                let left = Box::new(self.comparison_column(table_alias, column)?);

                let right = match value {
                    query_request::ComparisonValue::ScalarValueComparison { value, value_type } => {
                        Box::new(self.bind_parameter(BoundParam::Value {
                            value: value.to_owned(),
                            value_type: value_type.to_owned(),
                        }))
                    }
                    query_request::ComparisonValue::AnotherColumnComparison { column } => {
                        // technically, we could support column comparisons, but only if they don't cross relationships
                        // we can check the origin flag for this, to validate we're not traversing a relationship.
                        return Err(QueryBuilderError::RightHandColumnComparisonNotSupported(
                            column.name.to_owned(),
                        ));
                    }
                };

                use query_request::BinaryComparisonOperator::*;
                let expr = Expr::BinaryOp {
                    left,
                    right,
                    op: match operator {
                        LessThan => BinaryOperator::Lt,
                        LessThanOrEqual => BinaryOperator::LtEq,
                        Equal => BinaryOperator::Eq,
                        GreaterThan => BinaryOperator::Gt,
                        GreaterThanOrEqual => BinaryOperator::GtEq,
                    },
                };

                Ok((expr, vec![]))
            }
            query_request::Expression::BinaryArrayComparisonOperator {
                column,
                operator,
                value_type,
                values,
            } => {
                let expr = Box::new(self.comparison_column(table_alias, column)?);
                let list = values
                    .iter()
                    .map(|value| {
                        self.bind_parameter(BoundParam::Value {
                            value: value.to_owned(),
                            value_type: value_type.to_owned(),
                        })
                    })
                    .collect();

                let expr = match operator {
                    query_request::BinaryArrayComparisonOperator::In => Expr::InList { expr, list },
                };
                Ok((expr, vec![]))
            }
            query_request::Expression::Exists {
                in_table,
                selection,
            } => {
                if origin {
                    let join_alias = format!("_exists_{}", exists_index);
                    *exists_index += 1;

                    // assuming the only columns we care about are join columns.
                    // this may not be true if we support column comparison operators.
                    let (select_expr, join_expr, table_name, projection, group_by, limit) =
                        match in_table {
                            query_request::ExistsInTable::UnrelatedTable { table } => {
                                let left = Expr::CompoundIdentifier(vec![
                                    Ident::quoted(join_alias.clone()), // note: this is the alias of the join. Should be dynamic
                                    Ident::quoted("_exists"),
                                ]);
                                let right = Expr::Value(Value::Boolean(true));
                                let select_expr = Expr::BinaryOp {
                                    left: Box::new(left),
                                    op: BinaryOperator::Eq,
                                    right: Box::new(right),
                                };

                                let join_expr = Expr::Value(Value::Boolean(true));

                                let table_name = table;
                                let projection = vec![SelectItem::ExprWithAlias {
                                    expr: Expr::Value(Value::Boolean(true)),
                                    alias: Ident::quoted("_exists"),
                                }];
                                let group_by = vec![];
                                let limit = Some(Expr::Value(Value::Number("1".to_string())));
                                (
                                    select_expr,
                                    join_expr,
                                    table_name,
                                    projection,
                                    group_by,
                                    limit,
                                )
                            }
                            query_request::ExistsInTable::RelatedTable { relationship } => {
                                let relationship = self.table_relationship(table, relationship)?;
                                let select_expr = relationship
                                    .column_mapping
                                    .iter()
                                    .map(|(source_col, target_col)| {
                                        let left = Expr::CompoundIdentifier(vec![
                                            Ident::quoted(join_alias.clone()), // note: this is the alias of the join. Should be dynamic
                                            Ident::quoted(target_col),
                                        ]);
                                        let right = Expr::CompoundIdentifier(vec![
                                            Ident::quoted(table_alias), // should be alias of parent table
                                            Ident::quoted(source_col),
                                        ]);
                                        Expr::BinaryOp {
                                            left: Box::new(left),
                                            op: BinaryOperator::Eq,
                                            right: Box::new(right),
                                        }
                                    })
                                    .reduce(and_reducer)
                                    .map(|expr| match expr {
                                        Expr::BinaryOp {
                                            op: BinaryOperator::And,
                                            ..
                                        } => Expr::Nested(Box::new(expr)),
                                        _ => expr,
                                    })
                                    .unwrap_or(Expr::Value(Value::Boolean(true)));
                                let join_expr = select_expr.clone();

                                let table_name = &relationship.target_table;
                                let projection = relationship
                                    .column_mapping
                                    .iter()
                                    .map(|(_, target_col)| SelectItem::ExprWithAlias {
                                        expr: Expr::CompoundIdentifier(vec![
                                            Ident::quoted(join_alias.clone()),
                                            Ident::quoted(target_col),
                                        ]),
                                        alias: Ident::quoted(target_col),
                                    })
                                    .collect();
                                let group_by = relationship
                                    .column_mapping
                                    .iter()
                                    .map(|(_, target_col)| {
                                        Expr::CompoundIdentifier(vec![
                                            Ident::quoted(join_alias.clone()),
                                            Ident::quoted(target_col),
                                        ])
                                    })
                                    .collect();
                                let limit = None;

                                (
                                    select_expr,
                                    join_expr,
                                    table_name,
                                    projection,
                                    group_by,
                                    limit,
                                )
                            }
                        };

                    let mut subquery_exists_index = 0;

                    let (selection, joins) = self.selection_expression(
                        selection,
                        &mut subquery_exists_index,
                        false,
                        &join_alias,
                        table_name,
                    )?;

                    let from = vec![TableWithJoins {
                        relation: TableFactor::Table {
                            name: ObjectName(table_name.iter().map(|s| Ident::quoted(s)).collect()),
                            alias: Some(Ident::quoted(join_alias.clone())),
                        },
                        joins,
                    }];

                    let subquery = Query::new()
                        .projection(projection)
                        .from(from)
                        .predicate(Some(selection))
                        .group_by(group_by)
                        .limit(limit)
                        .boxed();

                    let join = Join {
                        join_operator: JoinOperator::LeftOuter(JoinConstraint::On(join_expr)),
                        relation: TableFactor::Derived {
                            subquery,
                            alias: Some(Ident::quoted(join_alias)),
                        },
                    };

                    Ok((select_expr, vec![join]))
                } else {
                    let join_alias = format!("{}.{}", table_alias, exists_index);
                    *exists_index += 1;

                    let (select_expr, join_expr, table_name) = match in_table {
                        query_request::ExistsInTable::UnrelatedTable { table } => {
                            let left = Expr::CompoundIdentifier(vec![
                                Ident::quoted(join_alias.clone()), // note: this is the alias of the join. Should be dynamic
                                Ident::quoted("_exists"),
                            ]);
                            let right = Expr::Value(Value::Boolean(true));
                            let select_expr = Expr::BinaryOp {
                                left: Box::new(left),
                                op: BinaryOperator::Eq,
                                right: Box::new(right),
                            };

                            let join_expr = Expr::Value(Value::Boolean(true));

                            let table_name = table;
                            (select_expr, join_expr, table_name)
                        }
                        query_request::ExistsInTable::RelatedTable { relationship } => {
                            let relationship = self.table_relationship(table, relationship)?;

                            let select_expr = relationship
                                .column_mapping
                                .iter()
                                .map(|(source_col, target_col)| {
                                    let left = Expr::CompoundIdentifier(vec![
                                        Ident::quoted(join_alias.clone()), // note: this is the alias of the join. Should be dynamic
                                        Ident::quoted(target_col),
                                    ]);
                                    let right = Expr::CompoundIdentifier(vec![
                                        Ident::quoted(table_alias), // should be alias of parent table
                                        Ident::quoted(source_col),
                                    ]);
                                    Expr::BinaryOp {
                                        left: Box::new(left),
                                        op: BinaryOperator::Eq,
                                        right: Box::new(right),
                                    }
                                })
                                .reduce(and_reducer)
                                .map(|expr| match expr {
                                    Expr::BinaryOp {
                                        op: BinaryOperator::And,
                                        ..
                                    } => Expr::Nested(Box::new(expr)),
                                    _ => expr,
                                })
                                .unwrap_or(Expr::Value(Value::Boolean(true)));
                            let join_expr = select_expr.clone();

                            let table_name = &relationship.target_table;

                            (select_expr, join_expr, table_name)
                        }
                    };

                    let (selection, joins) = self.selection_expression(
                        selection,
                        exists_index,
                        false,
                        &join_alias,
                        table_name,
                    )?;

                    let join = Join {
                        join_operator: JoinOperator::LeftOuter(JoinConstraint::On(join_expr)),
                        relation: TableFactor::Table {
                            name: ObjectName(table_name.iter().map(|s| Ident::quoted(s)).collect()),
                            alias: Some(Ident::quoted(join_alias)),
                        },
                    };

                    let joins = vec![join].into_iter().chain(joins).collect();

                    let select_expr = Expr::BinaryOp {
                        left: Box::new(select_expr),
                        op: BinaryOperator::And,
                        right: Box::new(selection),
                    };

                    Ok((select_expr, joins))
                }
            }
        }
    }
    fn comparison_column(
        &mut self,
        table_alias: &str,
        column: &query_request::ComparisonColumn,
    ) -> Result<Expr, QueryBuilderError> {
        if let Some(path) = &column.path {
            if !path.is_empty() {
                return Err(QueryBuilderError::UnsupportedColumnComparisonPath(
                    path.to_owned(),
                ));
            }
        }

        let expr = Expr::CompoundIdentifier(vec![
            Ident::quoted(table_alias),
            Ident::quoted(&column.name),
        ]);

        Ok(expr)
    }
    fn bind_parameter(&mut self, param: BoundParam) -> Expr {
        if self.bind_params {
            let placeholder_string = format!("__placeholder__{}", self.parameter_index);
            self.parameter_index += 1;
            self.parameters.insert(placeholder_string.clone(), param);
            Expr::Value(Value::Placeholder(placeholder_string))
        } else {
            match param {
                BoundParam::Number(number) => Expr::Value(Value::Number(number.to_string())),
                BoundParam::Value { value, value_type } => match value {
                    serde_json::Value::Number(number) => {
                        Expr::Value(Value::Number(number.to_string()))
                    }
                    serde_json::Value::String(string) => {
                        Expr::Value(Value::SingleQuotedString(string))
                    }
                    serde_json::Value::Bool(boolean) => Expr::Value(Value::Boolean(boolean)),
                    // feels like a hack.
                    serde_json::Value::Null => Expr::Value(Value::Null),
                    // note sure this works, should test
                    serde_json::Value::Array(_) => {
                        Expr::Value(Value::SingleQuotedString(value.to_string()))
                    }
                    serde_json::Value::Object(_) => {
                        Expr::Value(Value::SingleQuotedString(value.to_string()))
                    }
                },
            }
        }
    }
}
