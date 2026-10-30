/// Over the default limit, within the parser limit of 30.
pub fn tokenize(items: &[i32], strict: bool) -> i32 {
    let Some(first) = items.first() else {
        return 0;
    };
    let mut total = *first;
    for item in items {
        if *item > 0 {
            if strict && *item > 10 {
                total += 2;
            } else if *item > 5 {
                total += 1;
            } else {
                total -= 1;
            }
        } else {
            match item {
                -1 => total -= 1,
                _ => total -= 2,
            }
        }
    }
    while total > 100 {
        total /= 2;
    }
    if !strict || total < 0 {
        total = 0;
    }
    total
}
