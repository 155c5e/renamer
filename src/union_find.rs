//! Disjoint-set forest (union-find), shared by every clustering pass in the
//! app: near-duplicate images group by perceptual-hash proximity, and face
//! embeddings group into people by cosine similarity. Both need the same
//! thing — merge pairs that are "close enough", then read off the resulting
//! groups — so the algorithm lives here once instead of twice.

/// Disjoint-set forest with path halving and union by size, giving amortized
/// near-constant-time `find`/`union` even over tens of thousands of elements.
#[derive(Debug, Clone)]
pub struct UnionFind {
    parent: Vec<usize>,
    size: Vec<usize>,
}

impl UnionFind {
    pub fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            size: vec![1; n],
        }
    }

    pub fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    pub fn union(&mut self, a: usize, b: usize) {
        let (mut ra, mut rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        if self.size[ra] < self.size[rb] {
            std::mem::swap(&mut ra, &mut rb);
        }
        self.parent[rb] = ra;
        self.size[ra] += self.size[rb];
    }

    #[allow(dead_code)] // asserted by tests; no caller needs the count yet
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    #[allow(dead_code)] // asserted by tests
    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_find_unions_and_finds() {
        let mut uf = UnionFind::new(6);
        uf.union(0, 1);
        uf.union(1, 2);
        uf.union(4, 5);
        assert_eq!(uf.find(0), uf.find(2));
        assert_eq!(uf.find(4), uf.find(5));
        assert_ne!(uf.find(0), uf.find(3));
        assert_ne!(uf.find(0), uf.find(4));
        uf.union(0, 2); // idempotent
        assert_eq!(uf.find(0), uf.find(2));
    }

    #[test]
    fn every_element_starts_in_its_own_singleton_set() {
        let mut uf = UnionFind::new(4);
        let roots: Vec<usize> = (0..4).map(|i| uf.find(i)).collect();
        assert_eq!(roots, vec![0, 1, 2, 3]);
    }

    #[test]
    fn union_is_transitive_across_a_chain() {
        let mut uf = UnionFind::new(5);
        uf.union(0, 1);
        uf.union(1, 2);
        uf.union(2, 3);
        // 0 and 3 were never unioned directly, only through the chain.
        assert_eq!(uf.find(0), uf.find(3));
        assert_ne!(uf.find(0), uf.find(4));
    }

    #[test]
    fn empty_and_singleton_forests_behave() {
        let mut empty = UnionFind::new(0);
        assert!(empty.is_empty());
        let mut one = UnionFind::new(1);
        assert_eq!(one.find(0), 0);
        assert_eq!(one.len(), 1);
    }
}
