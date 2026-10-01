//! Small JavaScript-shaped helpers the service code leans on: an insertion-ordered map (a JS
//! `Map`), `String.prototype.localeCompare`, SHA-256 hex digests and bounded-concurrency
//! `Effect.forEach`.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::hash::Hash;

use futures::StreamExt;
use sha2::{Digest, Sha256};

/// A JS `Map`: iteration follows insertion order, [`OrderedMap::insert`] keeps the position of a
/// key already present (`map.set`), and [`OrderedMap::insert_last`] moves it to the end
/// (`map.delete(key); map.set(key, value)`).
#[derive(Debug, Clone)]
pub(crate) struct OrderedMap<K, V> {
    entries: HashMap<K, (u64, V)>,
    order: BTreeMap<u64, K>,
    next: u64,
}

impl<K, V> Default for OrderedMap<K, V> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            order: BTreeMap::new(),
            next: 0,
        }
    }
}

impl<K: Hash + Eq + Clone, V> OrderedMap<K, V> {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.entries.get(key).map(|(_, value)| value)
    }

    pub(crate) fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.entries.get_mut(key).map(|(_, value)| value)
    }

    pub(crate) fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.entries.contains_key(key)
    }

    /// `map.set(key, value)`: a key already present keeps its place.
    pub(crate) fn insert(&mut self, key: K, value: V) {
        if let Some((_, slot)) = self.entries.get_mut(&key) {
            *slot = value;
            return;
        }
        self.insert_last(key, value);
    }

    /// `map.delete(key); map.set(key, value)`: the key goes to the end.
    pub(crate) fn insert_last(&mut self, key: K, value: V) {
        self.remove(&key);
        let at = self.next;
        self.next += 1;
        self.order.insert(at, key.clone());
        self.entries.insert(key, (at, value));
    }

    /// Moves a present key to the end without changing its value.
    pub(crate) fn touch<Q>(&mut self, key: &Q)
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        if let Some((key, (at, value))) = self.entries.remove_entry(key) {
            self.order.remove(&at);
            self.insert_last(key, value);
        }
    }

    pub(crate) fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let (at, value) = self.entries.remove(key)?;
        self.order.remove(&at);
        Some(value)
    }

    /// Removes the oldest entry.
    pub(crate) fn pop_first(&mut self) -> Option<(K, V)> {
        let (_, key) = self.order.pop_first()?;
        let (_, value) = self.entries.remove(&key)?;
        Some((key, value))
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    /// Keys, oldest first.
    pub(crate) fn keys(&self) -> impl Iterator<Item = &K> {
        self.order.values()
    }

    /// Entries, oldest first.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.order.values().map(|key| (key, &self.entries[key].1))
    }

    /// Keeps the entries `keep` says yes to, in order.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        let dropped: Vec<K> = self.iter().filter(|(key, value)| !keep(key, value)).map(|(key, _)| key.clone()).collect();
        for key in dropped {
            self.remove(&key);
        }
    }
}

/// The position of an ASCII punctuation or symbol character in the root collation order
/// (`_ - , ; : ! ? . ' " ( ) [ ] { } @ * / \ & # % ` ^ + < = > | ~ $`).
fn punctuation_rank(character: char) -> Option<u32> {
    const ORDER: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    ORDER.find(character).map(|at| at as u32)
}

/// The primary weight of a character: white space, then punctuation and symbols, then digits,
/// then letters (case-insensitively).
fn primary_weight(character: char) -> (u8, u32) {
    if character.is_whitespace() {
        (0, character as u32)
    } else if let Some(rank) = punctuation_rank(character) {
        (1, rank)
    } else if character.is_numeric() {
        (2, character.to_digit(10).unwrap_or(character as u32))
    } else if character.is_alphabetic() {
        let lower = character.to_lowercase().next().unwrap_or(character);
        (3, lower as u32)
    } else {
        (1, 1_000 + character as u32)
    }
}

/// `left.localeCompare(right)` with the default (root) collation, close enough for the keys and
/// timestamps the service sorts: primary weights first, then lower case before upper case, then
/// code points.
pub(crate) fn locale_compare(left: &str, right: &str) -> Ordering {
    let primary = left.chars().map(primary_weight).cmp(right.chars().map(primary_weight));
    if primary != Ordering::Equal {
        return primary;
    }
    for (a, b) in left.chars().zip(right.chars()) {
        if a != b {
            let case = |c: char| u8::from(c.is_uppercase());
            return case(a).cmp(&case(b)).then(a.cmp(&b));
        }
    }
    left.len().cmp(&right.len())
}

/// SHA-256 of `text`, lowercase hex (`Encoding.encodeHex(crypto.digest("SHA-256", …))`).
pub(crate) fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// `Effect.forEach(items, f, {concurrency})`: at most `concurrency` at once, results in input
/// order.
pub(crate) async fn for_each_concurrent<I, F, Fut, T>(items: I, concurrency: usize, f: F) -> Vec<T>
where
    I: IntoIterator,
    F: FnMut(I::Item) -> Fut,
    Fut: Future<Output = T>,
{
    futures::stream::iter(items).map(f).buffered(concurrency.max(1)).collect().await
}

/// `Effect.firstSuccessOf`: tries each in turn and answers with the first success, or with the
/// last failure. `None` when there was nothing to try.
pub(crate) async fn first_success_of<T, E, Fut>(attempts: impl IntoIterator<Item = Fut>) -> Option<Result<T, E>>
where
    Fut: Future<Output = Result<T, E>>,
{
    let mut last = None;
    for attempt in attempts {
        match attempt.await {
            Ok(value) => return Some(Ok(value)),
            Err(error) => last = Some(Err(error)),
        }
    }
    last
}

/// `String.prototype.toLowerCase`.
pub(crate) fn lower(text: &str) -> String {
    text.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_map_keeps_insertion_order() {
        let mut map = OrderedMap::new();
        map.insert("a", 1);
        map.insert("b", 2);
        map.insert("a", 3);
        assert_eq!(map.keys().copied().collect::<Vec<_>>(), vec!["a", "b"]);
        map.insert_last("a", 4);
        assert_eq!(map.keys().copied().collect::<Vec<_>>(), vec!["b", "a"]);
        map.touch("b");
        assert_eq!(map.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>(), vec![("a", 4), ("b", 2)]);
        assert_eq!(map.pop_first(), Some(("a", 4)));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn locale_compare_orders_like_the_root_collation() {
        assert_eq!(locale_compare("2026-07-02T00:00:00Z", "2026-07-03T00:00:00Z"), Ordering::Less);
        assert_eq!(locale_compare("a", "B"), Ordering::Less);
        assert_eq!(locale_compare("a", "A"), Ordering::Less);
        assert_eq!(locale_compare("a_b", "a1"), Ordering::Less);
        assert_eq!(locale_compare("same", "same"), Ordering::Equal);
    }

    #[test]
    fn digests_as_lowercase_hex() {
        assert_eq!(sha256_hex("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
