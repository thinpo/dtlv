//! English Snowball stemmer (Porter2), the algorithm Datalevin's `:language "english"` analyzers use.
//! Non-ASCII words are returned lowercased and unstemmed.

fn vowel(c: u8) -> bool {
    matches!(c, b'a' | b'e' | b'i' | b'o' | b'u' | b'y')
}

fn ends(w: &[u8], suf: &str) -> bool {
    w.ends_with(suf.as_bytes())
}

/// Start of the region after the first non-vowel that follows a vowel, searching from `start`.
fn region_after(w: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < w.len() {
        if vowel(w[i - 1]) && !vowel(w[i]) {
            return i + 1;
        }
        i += 1;
    }
    w.len()
}

/// R1 and R2 of the word as it stood before step 1a; Porter2 keeps these fixed for later steps.
fn regions(w: &[u8]) -> (usize, usize) {
    let r1 = if w.starts_with(b"gener") || w.starts_with(b"arsen") {
        5
    } else if w.starts_with(b"commun") {
        6
    } else {
        region_after(w, 0)
    };
    let r1 = r1.min(w.len());
    let r2 = region_after(w, r1).min(w.len());
    (r1, r2)
}

/// A short syllable ends the word: non-vowel, vowel, non-vowel (not w, x, or Y), or a two-letter vowel-consonant word.
fn short_syllable_end(w: &[u8]) -> bool {
    let n = w.len();
    if n >= 3 && !vowel(w[n - 3]) && vowel(w[n - 2]) && !vowel(w[n - 1]) && !matches!(w[n - 1], b'w' | b'x' | b'Y') {
        return true;
    }
    n == 2 && vowel(w[0]) && !vowel(w[1])
}

fn step1a(w: &mut Vec<u8>) {
    if ends(w, "sses") {
        w.truncate(w.len() - 2);
    } else if ends(w, "ied") || ends(w, "ies") {
        if w.len() > 4 {
            w.truncate(w.len() - 2);
        } else {
            w.truncate(w.len() - 1);
        }
    } else if ends(w, "us") || ends(w, "ss") {
        // unchanged
    } else if ends(w, "s") && w[..w.len() - 2].iter().any(|&c| vowel(c)) {
        w.truncate(w.len() - 1);
    }
}

fn step1b(w: &mut Vec<u8>, r1: usize) {
    if ends(w, "eedly") || ends(w, "eed") {
        let len = if ends(w, "eedly") { 5 } else { 3 };
        if w.len() - len >= r1 {
            w.truncate(w.len() - len);
            w.extend_from_slice(b"ee");
        }
        return;
    }
    for suf in ["ingly", "edly", "ing", "ed"] {
        if !ends(w, suf) {
            continue;
        }
        let stem_len = w.len() - suf.len();
        if !w[..stem_len].iter().any(|&c| vowel(c)) {
            return;
        }
        w.truncate(stem_len);
        if ends(w, "at") || ends(w, "bl") || ends(w, "iz") {
            w.push(b'e');
        } else if w.len() >= 2 && w[w.len() - 1] == w[w.len() - 2] && matches!(w[w.len() - 1], b'b' | b'd' | b'f' | b'g' | b'm' | b'n' | b'p' | b'r' | b't') {
            w.pop();
        } else if regions(w).0 >= w.len() && short_syllable_end(w) {
            w.push(b'e');
        }
        return;
    }
}

fn step1c(w: &mut [u8]) {
    let n = w.len();
    if n >= 3 && matches!(w[n - 1], b'y' | b'Y') && !vowel(w[n - 2]) {
        w[n - 1] = b'i';
    }
}

/// Replaces the longest matching suffix in `table` if its stem is long enough and `allowed` accepts it.
/// Returns true when a rule fired, so later steps know the word changed.
fn replace_longest(
    w: &mut Vec<u8>,
    table: &[(&str, &str)],
    region: usize,
    allowed: impl Fn(&str, &[u8]) -> bool,
) -> bool {
    let Some(&(suf, rep)) = table.iter().filter(|(s, _)| ends(w, s)).max_by_key(|(s, _)| s.len()) else {
        return false;
    };
    let stem_len = w.len() - suf.len();
    if stem_len < region || !allowed(suf, &w[..stem_len]) {
        return false;
    }
    w.truncate(stem_len);
    w.extend_from_slice(rep.as_bytes());
    true
}

const STEP2: &[(&str, &str)] = &[
    ("tional", "tion"),
    ("enci", "ence"),
    ("anci", "ance"),
    ("abli", "able"),
    ("entli", "ent"),
    ("izer", "ize"),
    ("ization", "ize"),
    ("ational", "ate"),
    ("ation", "ate"),
    ("ator", "ate"),
    ("alism", "al"),
    ("aliti", "al"),
    ("alli", "al"),
    ("fulness", "ful"),
    ("ousli", "ous"),
    ("ousness", "ous"),
    ("iveness", "ive"),
    ("iviti", "ive"),
    ("biliti", "ble"),
    ("bli", "ble"),
    ("ogi", "og"),
    ("fulli", "ful"),
    ("lessli", "less"),
    ("li", ""),
];

const STEP3: &[(&str, &str)] = &[
    ("tional", "tion"),
    ("ational", "ate"),
    ("alize", "al"),
    ("icate", "ic"),
    ("iciti", "ic"),
    ("ical", "ic"),
    ("ful", ""),
    ("ness", ""),
    ("ative", ""),
];

const STEP4: &[(&str, &str)] = &[
    ("al", ""),
    ("ance", ""),
    ("ence", ""),
    ("er", ""),
    ("ic", ""),
    ("able", ""),
    ("ible", ""),
    ("ant", ""),
    ("ement", ""),
    ("ment", ""),
    ("ent", ""),
    ("ism", ""),
    ("ate", ""),
    ("iti", ""),
    ("ous", ""),
    ("ive", ""),
    ("ize", ""),
    ("ion", ""),
];

const EXCEPTIONS_1: &[(&str, &str)] = &[
    ("skis", "ski"),
    ("skies", "sky"),
    ("dying", "die"),
    ("lying", "lie"),
    ("tying", "tie"),
    ("idly", "idl"),
    ("gently", "gentl"),
    ("ugly", "ugli"),
    ("early", "earli"),
    ("only", "onli"),
    ("singly", "singl"),
    ("sky", "sky"),
    ("news", "news"),
    ("howe", "howe"),
    ("atlas", "atlas"),
    ("cosmos", "cosmos"),
    ("bias", "bias"),
    ("andes", "andes"),
];

const EXCEPTIONS_2: &[&str] = &[
    "inning", "outing", "canning", "herring", "earring", "proceed", "exceed", "succeed",
];

/// Stems one English word. The input is lowercased first.
pub fn stem_en(word: &str) -> String {
    let lower = word.to_lowercase();
    let ascii_word = lower.bytes().all(|c| c.is_ascii_lowercase() || c == b'\'');
    if lower.len() <= 2 || !ascii_word {
        return lower;
    }
    if let Some((_, r)) = EXCEPTIONS_1.iter().find(|(a, _)| *a == lower) {
        return (*r).to_owned();
    }
    let mut w: Vec<u8> = lower.into_bytes();
    if w[0] == b'\'' {
        w.remove(0);
    }
    for suf in ["'s'", "'s", "'"] {
        if ends(&w, suf) {
            w.truncate(w.len() - suf.len());
            break;
        }
    }
    if w.len() <= 2 {
        return String::from_utf8(w).unwrap_or_default();
    }
    // A y at the start, or after a vowel, is a consonant; mark it Y so step 1c and vowel tests skip it.
    if w[0] == b'y' {
        w[0] = b'Y';
    }
    for i in 1..w.len() {
        if w[i] == b'y' && vowel(w[i - 1]) {
            w[i] = b'Y';
        }
    }
    let (r1, r2) = regions(&w);

    step1a(&mut w);
    if EXCEPTIONS_2.iter().any(|e| e.as_bytes() == w.as_slice()) {
        return finish(w);
    }
    step1b(&mut w, r1);
    step1c(&mut w);

    // Step 2: only the longest suffix counts; `li` needs a valid li-ending before it, `ogi` needs `l`.
    replace_longest(&mut w, STEP2, r1, |suf, stem| match suf {
        "li" => matches!(stem.last(), Some(b'c' | b'd' | b'e' | b'g' | b'h' | b'k' | b'm' | b'n' | b'r' | b't')),
        "ogi" => stem.last() == Some(&b'l'),
        _ => true,
    });

    // Step 3: `ative` must lie in R2; the rest in R1.
    replace_longest(&mut w, STEP3, 0, |suf, stem| {
        if suf == "ative" { stem.len() >= r2 } else { stem.len() >= r1 }
    });

    // Step 4: deletions in R2; `ion` also needs an s or t before it.
    replace_longest(&mut w, STEP4, r2, |suf, stem| {
        suf != "ion" || matches!(stem.last(), Some(b's' | b't'))
    });

    step5(&mut w, r1, r2);
    finish(w)
}

fn step5(w: &mut Vec<u8>, r1: usize, r2: usize) {
    let n = w.len();
    if n >= 1 && w[n - 1] == b'e' {
        let stem = &w[..n - 1];
        if stem.len() >= r2 || (stem.len() >= r1 && !short_syllable_end(stem)) {
            w.pop();
        }
    } else if n >= 2 && w[n - 1] == b'l' && w[n - 2] == b'l' && n - 1 >= r2 {
        w.pop();
    }
}

fn finish(mut w: Vec<u8>) -> String {
    for c in &mut w {
        if *c == b'Y' {
            *c = b'y';
        }
    }
    String::from_utf8(w).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::stem_en;

    #[test]
    fn matches_snowball_english_reference_words() {
        let cases = [
            ("consign", "consign"), ("consigned", "consign"), ("running", "run"),
            ("caresses", "caress"), ("ponies", "poni"), ("ties", "tie"), ("cats", "cat"),
            ("feed", "feed"), ("agreed", "agre"), ("plastered", "plaster"), ("motoring", "motor"),
            ("sing", "sing"), ("conflated", "conflat"), ("troubled", "troubl"), ("sized", "size"),
            ("hopping", "hop"), ("falling", "fall"), ("hissing", "hiss"), ("fizzed", "fizz"),
            ("failing", "fail"), ("filing", "file"), ("happy", "happi"), ("relational", "relat"),
            ("conditional", "condit"), ("rational", "ration"), ("valenci", "valenc"),
            ("digitizer", "digit"), ("radicalli", "radic"), ("differentli", "differ"),
            ("operator", "oper"), ("feudalism", "feudal"), ("decisiveness", "decis"),
            ("hopefulness", "hope"), ("callousness", "callous"), ("formaliti", "formal"),
            ("sensitiviti", "sensit"), ("triplicate", "triplic"), ("formative", "format"),
            ("formalize", "formal"), ("electriciti", "electr"), ("electrical", "electr"),
            ("hopeful", "hope"), ("goodness", "good"), ("revival", "reviv"), ("allowance", "allow"),
            ("inference", "infer"), ("airliner", "airlin"), ("gyroscopic", "gyroscop"),
            ("adjustable", "adjust"), ("defensible", "defens"), ("irritant", "irrit"),
            ("replacement", "replac"), ("adjustment", "adjust"), ("dependent", "depend"),
            ("adoption", "adopt"), ("communism", "communism"), ("activate", "activ"),
            ("effective", "effect"), ("bowdlerize", "bowdler"), ("probate", "probat"),
            ("rate", "rate"), ("cease", "ceas"), ("controll", "control"), ("roll", "roll"),
            ("skis", "ski"), ("dying", "die"), ("sky", "sky"), ("gently", "gentl"), ("news", "news"),
            ("generalization", "general"), ("connections", "connect"), ("running", "run"),
        ];
        for (input, want) in cases {
            assert_eq!(stem_en(input), want, "stem of {input}");
        }
    }

    #[test]
    fn short_and_non_ascii_words_are_only_lowercased() {
        assert_eq!(stem_en("Go"), "go");
        assert_eq!(stem_en("naïve"), "naïve");
    }
}
