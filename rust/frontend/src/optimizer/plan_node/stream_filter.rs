// Copyright 2022 Singularity Data
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
//
use std::fmt;

use risingwave_common::catalog::Schema;
use risingwave_pb::stream_plan::stream_node::Node as ProstStreamNode;
use risingwave_pb::stream_plan::FilterNode;

use super::{LogicalFilter, PlanRef, PlanTreeNodeUnary, ToStreamProst};
use crate::optimizer::plan_node::PlanBase;
use crate::optimizer::property::{WithDistribution, WithSchema};
use crate::utils::Condition;

/// `StreamFilter` implements [`super::LogicalFilter`]
#[derive(Debug, Clone)]
pub struct StreamFilter {
    pub base: PlanBase,
    logical: LogicalFilter,
}

impl StreamFilter {
    pub fn new(logical: LogicalFilter) -> Self {
        let ctx = logical.base.ctx.clone();
        // TODO: derive from input
        let base = PlanBase::new_stream(
            ctx,
            logical.schema().clone(),
            logical.distribution().clone(),
        );
        StreamFilter { logical, base }
    }

    pub fn predicate(&self) -> &Condition {
        self.logical.predicate()
    }
}

impl fmt::Display for StreamFilter {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "StreamFilter {{ predicate: {} }}", self.predicate())
    }
}

impl PlanTreeNodeUnary for StreamFilter {
    fn input(&self) -> PlanRef {
        self.logical.input()
    }

    fn clone_with_input(&self, input: PlanRef) -> Self {
        Self::new(self.logical.clone_with_input(input))
    }
}

impl_plan_tree_node_for_unary! { StreamFilter }

impl WithSchema for StreamFilter {
    fn schema(&self) -> &Schema {
        self.logical.schema()
    }
}

impl ToStreamProst for StreamFilter {
    fn to_stream_prost_body(&self) -> ProstStreamNode {
        ProstStreamNode::FilterNode(FilterNode {
            search_condition: Some(self.predicate().as_expr().to_protobuf()),
        })
    }
}
