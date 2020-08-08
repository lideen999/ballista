// Copyright 2020 Andy Grove
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Filter operator.
use std::rc::Rc;
use std::cell::RefCell;
use std::sync::Arc;

use crate::arrow;
use crate::arrow::array;
use crate::arrow::datatypes::Schema;
use crate::datafusion::logicalplan::Expr;
use crate::error::{ballista_error, Result};
use crate::{
    cast_array,
    execution::physical_plan::{
        compile_expression, ColumnarBatch, ColumnarBatchIter, ColumnarBatchStream, ColumnarValue,
        ExecutionContext, ExecutionPlan, Expression, PhysicalPlan,
    },
};

use crate::execution::physical_plan::Partitioning;
use async_trait::async_trait;

/// FilterExec evaluates a boolean expression against each row of input to determine which rows
/// to include in output batches.
#[derive(Debug, Clone)]
pub struct FilterExec<'a> {
    pub(crate) child: Arc<PhysicalPlan<'a>>,
    pub(crate) filter_expr: Arc<Expr>,
}

impl FilterExec<'_> {
    pub fn new(child: &PhysicalPlan, filter_expr: &Expr) -> Self {
        Self {
            child: Arc::new(child.clone()),
            filter_expr: Arc::new(filter_expr.clone()),
        }
    }

    pub fn with_new_children(&self, new_children: Vec<Arc<PhysicalPlan>>) -> FilterExec {
        assert!(new_children.len() == 1);
        FilterExec {
            filter_expr: self.filter_expr.clone(),
            child: new_children[0].clone(),
        }
    }
}

#[async_trait]
impl<'a> ExecutionPlan<'a> for FilterExec<'a> {
    fn schema(&self) -> Arc<Schema> {
        // a filter does not alter the schema
        self.child.as_execution_plan().schema()
    }

    fn output_partitioning(&self) -> Partitioning {
        self.child.as_execution_plan().output_partitioning()
    }

    fn children(&self) -> Vec<Arc<PhysicalPlan>> {
        vec![self.child.clone()]
    }

    async fn execute(
        &self,
        ctx: Arc<dyn ExecutionContext>,
        partition_index: usize,
    ) -> Result<ColumnarBatchStream<'a>> {
        let expr = compile_expression(&self.filter_expr, &self.schema())?;
        let arc = self
            .child
            .as_execution_plan();
        let x = arc
            .execute(ctx.clone(), partition_index)
            .await?;
        Ok(Rc::new(RefCell::new(FilterIter {
            input_plan: arc,
            input: x,
            filter_expr: expr,
        })))
    }
}

#[allow(dead_code)]
struct FilterIter<'a> {
    input_plan: Arc<dyn ExecutionPlan<'a> + 'a>,
    input: ColumnarBatchStream<'a>,
    filter_expr: Arc<dyn Expression>,
}

impl ColumnarBatchIter for FilterIter<'_> {
    fn schema(&self) -> Arc<Schema> {
        self.input.schema()
    }

    fn next(&self) -> Result<Option<ColumnarBatch>> {
        let input = self.input.borrow();
        match input.next()? {
            Some(input) => {
                let bools = self.filter_expr.evaluate(&input)?;
                let batch = apply_filter(&input, &bools, self.input.schema())?;
                Ok(Some(batch))
            }
            None => Ok(None),
        }
    }
}

/// Filter the provided batch based on the bitmask
fn apply_filter(
    batch: &ColumnarBatch,
    bitmask: &ColumnarValue,
    schema: Arc<Schema>,
) -> Result<ColumnarBatch> {
    let predicate = bitmask.to_arrow()?;
    let predicate = cast_array!(predicate, BooleanArray)?;

    let mut filtered_arrays = Vec::with_capacity(batch.num_columns());
    for i in 0..batch.num_columns() {
        let array = batch.column(i);
        let filtered_array = arrow::compute::filter(array.to_arrow()?.as_ref(), predicate)?;
        filtered_arrays.push(ColumnarValue::Columnar(filtered_array));
    }

    Ok(ColumnarBatch::from_values_and_schema(
        &filtered_arrays,
        schema,
    ))
}
