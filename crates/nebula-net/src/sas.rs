//! Short authentication strings: four words both devices derive from the
//! secure handshake, so a person can compare them across the two screens.
//!
//! The words come from the Noise handshake hash, which commits to both
//! devices' keys and the whole handshake transcript. A man-in-the-middle
//! would have to run a separate handshake with each side, giving each side a
//! different hash and different words. This is the same idea as the short
//! authentication string in ZRTP or the emoji check in encrypted calls.
//! Four words from 256 give 32 bits: a middleman's chance of both sides
//! showing the same words by luck is one in about four billion.

use std::fmt;

const CONTEXT: &str = "nebula 2026-11 security code v1";

/// Four words that match on both devices if, and only if, they share the
/// same encrypted session.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SecurityCode([&'static str; 4]);

impl SecurityCode {
    pub fn from_handshake_hash(hash: &[u8]) -> Self {
        let key = blake3::derive_key(CONTEXT, hash);
        Self([0, 1, 2, 3].map(|i| WORDS[usize::from(key[i])]))
    }

    pub fn words(&self) -> [&'static str; 4] {
        self.0
    }
}

impl fmt::Display for SecurityCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.join(" "))
    }
}

/// 256 short, concrete, distinct words, one per byte value.
const WORDS: [&str; 256] = [
    "acorn", "anchor", "apple", "arrow", "atlas", "badge", "bagel", "bamboo", "banjo", "barrel",
    "basil", "beacon", "beaver", "bell", "berry", "bicycle", "bison", "blanket", "blossom", "boat",
    "bonfire", "bottle", "bramble", "breeze", "brick", "bridge", "bronze", "brook", "bubble",
    "bucket", "buffalo", "bugle", "butter", "button", "cabin", "cactus", "camel", "candle",
    "canoe", "canyon", "captain", "carrot", "castle", "cedar", "cello", "chalk", "cherry", "chess",
    "cider", "circus", "clover", "coconut", "comet", "compass", "copper", "coral", "cotton",
    "cowboy", "coyote", "crayon", "cricket", "crystal", "cupcake", "daisy", "desert", "diamond",
    "dolphin", "donkey", "dragon", "drum", "eagle", "easel", "echo", "elbow", "ember", "emerald",
    "falcon", "feather", "fern", "fiddle", "fig", "firefly", "flame", "flute", "forest", "fossil",
    "fountain", "fox", "galaxy", "garden", "garlic", "gazelle", "geyser", "ginger", "giraffe",
    "glacier", "globe", "goblet", "goose", "granite", "grape", "gravel", "guitar", "hammer",
    "harbor", "harp", "hazel", "helmet", "heron", "hickory", "honey", "horizon", "husky",
    "iceberg", "igloo", "island", "ivory", "jacket", "jaguar", "jasmine", "jelly", "jigsaw",
    "jungle", "kayak", "kettle", "kitten", "koala", "ladder", "lagoon", "lantern", "lasso",
    "lemon", "leopard", "lettuce", "lily", "lime", "lizard", "llama", "lobster", "locket", "lotus",
    "magnet", "mango", "maple", "marble", "meadow", "melon", "mitten", "moose", "mosaic", "moth",
    "muffin", "mustard", "nectar", "needle", "nest", "nickel", "noodle", "nutmeg", "oak", "oasis",
    "ocean", "olive", "onion", "orbit", "orchid", "otter", "owl", "paddle", "panda", "panther",
    "papaya", "parrot", "peach", "peanut", "pebble", "pelican", "pencil", "pepper", "piano",
    "pickle", "pigeon", "pillow", "pine", "pirate", "planet", "plum", "pony", "popcorn", "potato",
    "pretzel", "puddle", "pumpkin", "puzzle", "quail", "quartz", "quill", "rabbit", "raccoon",
    "radish", "rainbow", "raven", "reef", "ribbon", "river", "robin", "rocket", "ruby", "saddle",
    "salmon", "sandal", "scarf", "shadow", "silver", "sketch", "sled", "sparrow", "spider",
    "sponge", "spruce", "squirrel", "statue", "stone", "swan", "teapot", "thimble", "thistle",
    "thunder", "tiger", "toast", "tomato", "topaz", "tortoise", "trumpet", "tulip", "tundra",
    "turnip", "turtle", "umbrella", "valley", "velvet", "violet", "volcano", "waffle", "walnut",
    "walrus", "whistle", "willow", "window", "wizard", "wolf", "yarn", "yogurt", "zebra", "zinnia",
    "zipper",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn words_are_distinct_and_plain() {
        let set: HashSet<&str> = WORDS.iter().copied().collect();
        assert_eq!(set.len(), 256);
        for w in WORDS {
            assert!(
                w.len() <= 10 && w.chars().all(|c| c.is_ascii_lowercase()),
                "{w}"
            );
        }
    }

    #[test]
    fn same_hash_same_code_different_hash_different_code() {
        let a = SecurityCode::from_handshake_hash(&[1; 32]);
        assert_eq!(a, SecurityCode::from_handshake_hash(&[1; 32]));
        assert_ne!(a, SecurityCode::from_handshake_hash(&[2; 32]));
        assert_eq!(a.to_string().split(' ').count(), 4);
    }
}
