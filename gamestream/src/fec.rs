//! Rebuilds lost video packets from parity packets. The host's code is a
//! systematic Cauchy Reed-Solomon code over GF(2^8) with the polynomial
//! 0x11D: the parity shard of row `j` is the sum over the data shards `i` of
//! `shard_i / ((parity_count + i) ^ j)`.

use std::sync::LazyLock;

struct Field {
    /// `multiply[a][b]` is the product of `a` and `b`.
    multiply: Vec<[u8; 256]>,
    inverse: [u8; 256],
}

static FIELD: LazyLock<Field> = LazyLock::new(|| {
    let (mut exp, mut log) = ([0u8; 510], [0usize; 256]);
    let mut value = 1u16;
    for power in 0..255 {
        exp[power] = value as u8;
        exp[power + 255] = value as u8;
        log[value as usize] = power;
        value <<= 1;
        if value & 0x100 != 0 { value ^= 0x11d; }
    }
    let mut multiply = vec![[0u8; 256]; 256];
    let mut inverse = [0u8; 256];
    for a in 1..256 {
        inverse[a] = exp[255 - log[a]];
        for b in 1..256 { multiply[a][b] = exp[log[a] + log[b]]; }
    }
    Field { multiply, inverse }
});

/// The factor of data shard `data` in parity shard `row`.
fn factor(parity_count: usize, row: usize, data: usize) -> u8 {
    FIELD.inverse[(parity_count + data) ^ row]
}

/// `target += factor * source`.
fn add_scaled(target: &mut [u8], source: &[u8], factor: u8) {
    let products = &FIELD.multiply[factor as usize];
    for (target, source) in target.iter_mut().zip(source) { *target ^= products[*source as usize]; }
}

fn scale(target: &mut [u8], factor: u8) {
    let products = &FIELD.multiply[factor as usize];
    for byte in target { *byte = products[*byte as usize]; }
}

/// Fills in the missing data shards. `shards` holds the data shards and then
/// the parity shards, `None` where the packet is lost, all of one length and
/// 255 at most. Returns false when too few shards arrived.
pub(crate) fn recover(shards: &mut [Option<Vec<u8>>], data_count: usize) -> bool {
    let parity_count = shards.len() - data_count;
    let missing: Vec<usize> = (0..data_count).filter(|&index| shards[index].is_none()).collect();
    let rows: Vec<usize> = (0..parity_count).filter(|&row| shards[data_count + row].is_some())
        .take(missing.len()).collect();
    if rows.len() < missing.len() { return false; }

    // Each parity shard, less what the received data shards put into it, is a
    // sum of the missing shards alone: one equation per parity shard.
    let mut sums: Vec<Vec<u8>> = rows.iter().map(|&row| {
        let mut sum = shards[data_count + row].clone().unwrap();
        for (index, shard) in shards[..data_count].iter().enumerate() {
            if let Some(shard) = shard { add_scaled(&mut sum, shard, factor(parity_count, row, index)); }
        }
        sum
    }).collect();
    let mut matrix: Vec<Vec<u8>> = rows.iter()
        .map(|&row| missing.iter().map(|&index| factor(parity_count, row, index)).collect())
        .collect();

    // Gauss-Jordan elimination. Every square part of a Cauchy matrix can be
    // inverted, so a pivot is always found.
    for pivot in 0..missing.len() {
        let Some(found) = (pivot..missing.len()).find(|&row| matrix[row][pivot] != 0) else { return false };
        matrix.swap(pivot, found);
        sums.swap(pivot, found);
        let inverse = FIELD.inverse[matrix[pivot][pivot] as usize];
        scale(&mut matrix[pivot], inverse);
        scale(&mut sums[pivot], inverse);
        for row in (0..missing.len()).filter(|&row| row != pivot) {
            let factor = matrix[row][pivot];
            if factor == 0 { continue; }
            let (pivot_row, pivot_sum) = (matrix[pivot].clone(), sums[pivot].clone());
            add_scaled(&mut matrix[row], &pivot_row, factor);
            add_scaled(&mut sums[row], &pivot_sum, factor);
        }
    }
    for (index, shard) in missing.into_iter().zip(sums) { shards[index] = Some(shard); }
    true
}

/// The parity shards of `data`, as the host makes them.
#[cfg(test)]
pub(crate) fn encode(data: &[Vec<u8>], parity_count: usize) -> Vec<Vec<u8>> {
    (0..parity_count).map(|row| {
        let mut parity = vec![0u8; data[0].len()];
        for (index, shard) in data.iter().enumerate() { add_scaled(&mut parity, shard, factor(parity_count, row, index)); }
        parity
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_the_matrix_of_the_hosts_library() {
        // From the library's own tables: 1/2, 1/3, 1/4 and 1/5.
        let row = |row| (0..4).map(|data| factor(2, row, data)).collect::<Vec<u8>>();
        assert_eq!(row(0), [142, 244, 71, 167]);
        assert_eq!(row(1), [244, 142, 167, 71]);
        assert_eq!(FIELD.multiply[2][142], 1);
        assert_eq!(FIELD.multiply[0x80][2], 0x1d);
    }

    fn block(data_count: usize, parity_count: usize) -> Vec<Option<Vec<u8>>> {
        let data: Vec<Vec<u8>> = (0..data_count)
            .map(|shard| (0..37).map(|byte| (shard * 31 + byte * 7 + 1) as u8).collect())
            .collect();
        let parity = encode(&data, parity_count);
        data.into_iter().chain(parity).map(Some).collect()
    }

    #[test]
    fn rebuilds_as_many_data_shards_as_parity_shards_arrived() {
        let whole = block(10, 3);
        let mut shards = whole.clone();
        // Two data shards and the first parity shard are lost.
        for lost in [0, 7, 10] { shards[lost] = None; }
        assert!(recover(&mut shards, 10));
        assert_eq!(shards[..10], whole[..10]);

        let mut shards = whole.clone();
        for lost in [1, 2, 3] { shards[lost] = None; }
        assert!(recover(&mut shards, 10));
        assert_eq!(shards[..10], whole[..10]);
    }

    #[test]
    fn refuses_when_more_is_lost_than_parity_arrived() {
        let mut shards = block(10, 3);
        for lost in [0, 4, 9, 11] { shards[lost] = None; }
        assert!(!recover(&mut shards, 10));
        assert!(shards[0].is_none());
        // Nothing to do when all data arrived.
        let mut shards = block(3, 1);
        shards[3] = None;
        assert!(recover(&mut shards, 3));
    }

    #[test]
    fn rebuilds_a_block_of_the_largest_size() {
        let whole = block(213, 42);
        let mut shards = whole.clone();
        for lost in (0..213).step_by(6) { shards[lost] = None; }
        shards[213] = None;
        assert!(recover(&mut shards, 213));
        assert_eq!(shards[..213], whole[..213]);
    }
}
