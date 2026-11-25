//! Read-only entity views. An `Entity` reads attributes on demand; `touch` materializes all of them.
//!
//! Cardinality-many attributes are returned as a `Value::Tuple` of their values, in stored order.

use std::collections::BTreeMap;

use dtlv_codec::{EntityId, Value};
use dtlv_kv::ReadTxn;

use crate::{Conn, DatalogError};

pub struct Entity<'a, 'r> {
    conn: &'a Conn,
    rtxn: &'a ReadTxn<'r>,
    eid: EntityId,
}

impl<'a, 'r> Entity<'a, 'r> {
    pub fn eid(&self) -> EntityId {
        self.eid
    }

    /// One attribute. Returns `None` when the entity has no value for it.
    pub fn get(&self, attr: &str) -> Result<Option<Value>, DatalogError> {
        let aid = self.conn.attr_id(attr)?;
        if self.conn.attrs.get(&aid).is_some_and(|s| s.cardinality == crate::Cardinality::Many) {
            let vals = self.conn.get_attr_many(self.rtxn, self.eid, attr)?;
            return Ok((!vals.is_empty()).then(|| Value::Tuple(vals)));
        }
        self.conn.get_attr(self.rtxn, self.eid, attr)
    }

    /// Every attribute of the entity. Attributes that are not in the current schema are included.
    pub fn touch(&self) -> Result<BTreeMap<String, Value>, DatalogError> {
        let mut many: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        let mut one: BTreeMap<String, Value> = BTreeMap::new();
        for (_, attr, val) in self.conn.search_datoms(self.rtxn, Some(self.eid), None, None)? {
            if self.conn.attrs.get(&self.conn.attr_id(&attr)?).is_some_and(|s| s.cardinality == crate::Cardinality::Many) {
                many.entry(attr).or_default().push(val);
            } else {
                one.insert(attr, val);
            }
        }
        for (attr, vals) in many {
            one.insert(attr, Value::Tuple(vals));
        }
        Ok(one)
    }
}

impl Conn {
    /// An entity view for `eid`. Nothing is read until an attribute is asked for.
    pub fn entity<'a, 'r>(&'a self, rtxn: &'a ReadTxn<'r>, eid: EntityId) -> Entity<'a, 'r> {
        Entity { conn: self, rtxn, eid }
    }

    /// The entity whose unique attribute `attr` equals `val`, if any.
    pub fn entity_by_lookup<'a, 'r>(
        &'a self,
        rtxn: &'a ReadTxn<'r>,
        attr: &str,
        val: &Value,
    ) -> Result<Option<Entity<'a, 'r>>, DatalogError> {
        Ok(self.lookup_entity_by_av(rtxn, attr, val)?.map(|eid| Entity { conn: self, rtxn, eid }))
    }
}
