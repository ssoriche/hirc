//! IRCv3 batches as they nest: which batch a line is in, what kind each is,
//! and which of our labels (labeled-response) it answers. A line in a
//! `chathistory` batch inside a `labeled-response` batch is history that
//! answers the outer batch's label. Bounded: a server can't make us hold
//! unbounded state or walk a loop of refs forever.

use crate::proto::Message;
use std::collections::HashMap;

/// Batches open at once past this are ignored.
pub const MAX_OPEN: usize = 64;
/// How far up the nesting a walk goes (it also ends a loop of refs).
pub const MAX_DEPTH: usize = 8;

/// One open batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Open {
    /// `chathistory`, `draft/multiline`, `labeled-response`, ...
    pub kind: String,
    /// The params after the type: a history batch's target.
    pub params: Vec<String>,
    /// The batch this one is inside (its own `batch` tag).
    pub parent: Option<String>,
    /// Its `label` tag, when it answers a command we labeled.
    pub label: Option<String>,
}

#[derive(Debug, Default)]
pub struct Batches {
    open: HashMap<String, Open>,
}

impl Batches {
    /// Forget everything, for a new connection.
    pub fn clear(&mut self) {
        self.open.clear();
    }

    /// `BATCH +ref type params...`: remember it. False, and nothing kept,
    /// for a malformed open, or for a new ref while `MAX_OPEN` are open.
    pub fn open(&mut self, msg: &Message) -> bool {
        let (Some(r), Some(kind)) = (msg.params.first().and_then(|p| p.strip_prefix('+')), msg.params.get(1)) else { return false };
        if r.is_empty() || (self.open.len() >= MAX_OPEN && !self.open.contains_key(r)) {
            return false;
        }
        let tag = |k: &str| msg.tag(k).filter(|v| !v.is_empty()).map(str::to_owned);
        self.open.insert(r.to_owned(), Open { kind: kind.clone(), params: msg.params[2..].to_vec(), parent: tag("batch"), label: tag("label") });
        true
    }

    /// `BATCH -ref`: what was open under it, if anything.
    pub fn close(&mut self, r: &str) -> Option<Open> {
        self.open.remove(r)
    }

    pub fn get(&self, r: &str) -> Option<&Open> {
        self.open.get(r)
    }

    /// `r` itself, or the nearest open batch around it, of type `kind`.
    pub fn enclosing(&self, r: &str, kind: &str) -> Option<String> {
        let mut cur = r;
        for _ in 0..MAX_DEPTH {
            let b = self.open.get(cur)?;
            if b.kind == kind {
                return Some(cur.to_owned());
            }
            cur = b.parent.as_deref()?;
        }
        None
    }

    /// The label `r` answers: its own, or the nearest one around it.
    pub fn label(&self, r: &str) -> Option<String> {
        let mut cur = r;
        for _ in 0..MAX_DEPTH {
            let b = self.open.get(cur)?;
            if let Some(l) = &b.label {
                return Some(l.clone());
            }
            cur = b.parent.as_deref()?;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(b: &mut Batches, line: &str) -> bool {
        b.open(&Message::parse(line).unwrap())
    }

    #[test]
    fn history_inside_a_labeled_response_answers_its_label() {
        let mut b = Batches::default();
        assert!(open(&mut b, "@label=hl1 :srv BATCH +lr labeled-response"));
        assert!(open(&mut b, "@batch=lr :srv BATCH +h chathistory #test"));
        assert!(open(&mut b, "@batch=h;msgid=m1 :bob!b@h BATCH +ml draft/multiline #test"));
        assert_eq!(b.enclosing("ml", "chathistory").as_deref(), Some("h"));
        assert_eq!(b.enclosing("h", "chathistory").as_deref(), Some("h"));
        assert_eq!(b.enclosing("h", "draft/multiline"), None);
        assert_eq!(b.label("ml").as_deref(), Some("hl1"));
        assert_eq!(b.get("h").unwrap().params, ["#test"]);
        assert_eq!(b.get("h").unwrap().parent.as_deref(), Some("lr"));
    }

    #[test]
    fn a_label_on_the_history_batch_itself() {
        let mut b = Batches::default();
        open(&mut b, "@label=hl2 :srv BATCH +h chathistory bob");
        assert_eq!(b.label("h").as_deref(), Some("hl2"));
        assert_eq!(b.close("h").map(|o| o.kind), Some("chathistory".to_owned()));
        assert_eq!(b.get("h"), None);
        assert_eq!(b.label("h"), None);
    }

    #[test]
    fn unknown_refs_and_loops_end_the_walk() {
        let mut b = Batches::default();
        assert_eq!(b.enclosing("nope", "chathistory"), None);
        // A server that names each batch the other's parent.
        open(&mut b, "@batch=y :srv BATCH +x netsplit");
        open(&mut b, "@batch=x :srv BATCH +y netjoin");
        assert_eq!(b.enclosing("x", "chathistory"), None);
        assert_eq!(b.label("y"), None);
    }

    #[test]
    fn malformed_opens_are_refused() {
        let mut b = Batches::default();
        for line in ["BATCH", "BATCH +", "BATCH +x", "BATCH -x chathistory", "BATCH x chathistory"] {
            assert!(!open(&mut b, line), "{line}");
        }
    }

    #[test]
    fn open_batches_are_bounded() {
        let mut b = Batches::default();
        for i in 0..MAX_OPEN {
            assert!(open(&mut b, &format!("BATCH +r{i} netsplit")));
        }
        assert!(!open(&mut b, "BATCH +one-too-many netsplit"));
        // Reopening a ref that is already open is not a new one.
        assert!(open(&mut b, "BATCH +r0 chathistory #a"));
        b.clear();
        assert!(open(&mut b, "BATCH +fresh netsplit"));
    }
}
