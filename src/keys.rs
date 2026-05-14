use std::{error::Error as StdError, str::FromStr};

use color_eyre::eyre;

///
/// Smooth Weighted Round Robin
///
#[derive(Debug, Default)]
pub struct KeyRepo<T> {
    keys: Vec<KeyEntry<T>>,
}

#[derive(Debug)]
pub struct KeyEntry<T> {
    key: T,
    weight: u32,
    current: i32,
}

impl<T> KeyRepo<T> {
    pub fn try_from_str(s: &str) -> eyre::Result<Self>
    where
        T: FromStr<Err: StdError + Send + Sync + 'static>,
    {
        let mut keys = Vec::new();

        for pair in s.trim().split(',') {
            let Some((key, weight)) = pair.trim().split_once(':') else {
                eyre::bail!("{pair} is not in format 'key:weight'");
            };

            let weight = weight.trim().parse::<u32>()?;

            if weight == 0 {
                eyre::bail!("weight must be > 0");
            }

            keys.push(KeyEntry {
                key: T::from_str(key.trim())?,
                weight,
                current: 0,
            });
        }

        Ok(Self { keys })
    }

    pub fn add(&mut self, key: T, weight: u32) {
        assert!(weight > 0, "weight must be > 0");
        self.keys.push(KeyEntry {
            key,
            weight,
            current: 0,
        });
    }

    pub fn next(&mut self) -> Option<&T> {
        if self.keys.is_empty() {
            return None;
        }

        let total: u32 = self.keys.iter().map(|e| e.weight).sum();

        let mut best = 0;
        for i in 0..self.keys.len() {
            self.keys[i].current += self.keys[i].weight as i32;
            if self.keys[i].current > self.keys[best].current {
                best = i;
            }
        }

        self.keys[best].current -= total as i32;

        Some(&self.keys[best].key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::{fixture, rstest};

    #[rstest]
    #[case::single("a:1", vec![("a", 1)])]
    #[case::multiple("a:1,b:2", vec![("a", 1), ("b", 2)])]
    #[case::whitespace_single("  a : 1", vec![("a", 1)])]
    #[case::whitespace_multiple("  a : 1 , b : 10", vec![("a", 1), ("b", 10)])]
    fn parsing_works(#[case] input: String, #[case] expected: Vec<(&str, u32)>) {
        let repo = KeyRepo::<String>::try_from_str(&input).unwrap();

        let actual: Vec<_> = repo
            .keys
            .iter()
            .map(|e| (e.key.as_str(), e.weight))
            .collect();

        let expected: Vec<_> = expected
            .iter()
            .map(|(key, weight)| (*key, *weight))
            .collect();

        assert_eq!(actual, expected);
    }

    #[rstest]
    #[case::zero_weight("a:0")]
    #[case::no_colon("a;1")]
    #[case::no_weight("a:")]
    #[case::non_numeric("a:abc")]
    #[case::semicolon("a:1;b:2")]
    #[case::empty_pair("a:1,,b:2")]
    fn parsing_fails(#[case] input: String) {
        assert!(KeyRepo::<String>::try_from_str(&input).is_err());
    }

    #[fixture]
    fn key_repo() -> KeyRepo<&'static str> {
        KeyRepo::default()
    }

    #[rstest]
    fn next_on_empty_returns_none(mut key_repo: KeyRepo<&str>) {
        assert!(key_repo.next().is_none());
    }

    #[rstest]
    fn next_follows_smooth_wrr(mut key_repo: KeyRepo<&str>) {
        key_repo.add("a", 5);
        key_repo.add("b", 1);
        key_repo.add("c", 1);

        let seq: Vec<_> = (0..7).map(|_| *key_repo.next().unwrap()).collect();
        assert_eq!(seq, vec!["a", "a", "b", "a", "c", "a", "a"]);
    }

    #[rstest]
    fn next_follows_weight_distribution(mut key_repo: KeyRepo<&str>) {
        key_repo.add("a", 3);
        key_repo.add("b", 1);

        let n = 4000;
        let mut count_a = 0;
        for _ in 0..n {
            if *key_repo.next().unwrap() == "a" {
                count_a += 1;
            }
        }

        assert_eq!(count_a, 3 * n / 4); // 3:1
    }

    #[rstest]
    fn next_is_periodic(mut key_repo: KeyRepo<&str>) {
        key_repo.add("a", 2);
        key_repo.add("b", 3);
        key_repo.add("c", 1);

        let first: Vec<_> = (0..6).map(|_| *key_repo.next().unwrap()).collect();
        let second: Vec<_> = (0..6).map(|_| *key_repo.next().unwrap()).collect();

        assert_eq!(first, second);
    }
}
