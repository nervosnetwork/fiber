use super::*;

impl InternalResult {
    pub fn fail_range_pairs(&mut self, nodes: &[SessionRouteNode], start: usize, end: usize) {
        for index in start.max(1)..=end {
            self.fail_pair(nodes, index);
        }
    }
}
