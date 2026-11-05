// Same-resolution rate control of TICO RAW codestreams on the GPU: the work of
// `requant::requant_to_budget`, one workgroup per slice, for frames whose lossless repack
// does not fit their budget. Every step mirrors the CPU code and gives the same bytes:
// entropy decode of each precinct (unary GCLI codes found in parallel from the positions of
// their terminating zero bits, magnitudes and signs placed by prefix sums), the rank search
// of `rate::finest_fitting` on exact candidate sizes, and the entropy encode of the chosen
// rank. Anything unexpected sets an error and the host codes the frame on the CPU instead.

const WG: u32 = 256u;
const TMAX: i32 = 15;
const SIG_GROUP: u32 = 8u;

// Parameters at the start of `tab` (see requant_wgpu.rs).
const P_NB: u32 = 0u;
const P_NLINES: u32 = 1u;
const P_NPK: u32 = 2u;
const P_G: u32 = 3u;
const P_PRECINCTS: u32 = 4u;
const P_SLICEH: u32 = 5u;
const P_HLEN: u32 = 7u;
const P_HI: u32 = 8u;
const P_SEEDED: u32 = 9u;
const P_START: u32 = 10u;
const P_SG: u32 = 11u;
const P_CAP: u32 = 12u;
const P_CSLEN: u32 = 13u;
const P_LINES: u32 = 14u;
const P_PKS: u32 = 15u;
const P_G2L: u32 = 16u;
const P_SG2L: u32 = 17u;
const P_NSG: u32 = 18u;
const P_STRIDE: u32 = 19u;
const P_SLICES: u32 = 6u;
const LINE_WORDS: u32 = 12u;

@group(0) @binding(0) var<storage, read> tab: array<u32>;
@group(0) @binding(1) var<storage, read> cs: array<u32>;
@group(0) @binding(2) var<storage, read> fm: array<u32>;
@group(0) @binding(3) var<storage, read_write> outw: array<atomic<u32>>;
@group(0) @binding(4) var<storage, read_write> res: array<u32>;
@group(0) @binding(5) var<storage, read_write> scr: array<u32>;
// Frames batched into one dispatch: per frame (workgroup_id.y), the word offsets of its
// parts of tab, cs, fm, outw, res and scr, and its slice count.
@group(0) @binding(6) var<storage, read> frames: array<u32>;
const FRAME_WORDS: u32 = 8u;
var<private> fT: u32;
var<private> fC: u32;
var<private> fM: u32;
var<private> fO: u32;
var<private> fR: u32;
var<private> fS: u32;

var<workgroup> wscan: array<vec4<u32>, 256>;
var<workgroup> cam_mode: array<u32, 32>;
var<workgroup> out_mode: array<u32, 32>;
var<workgroup> dec_gtli: array<i32, 32>;
var<workgroup> dec_valid: array<u32, 32>;
var<workgroup> enc_gtli: array<i32, 32>;
var<workgroup> enc_valid: array<u32, 32>;
var<workgroup> line_t0: array<i32, 64>;
var<workgroup> line_t1: array<i32, 64>;
var<workgroup> line_sigoff: array<u32, 64>;
var<workgroup> line_cost: array<atomic<u32>, 256>;
var<workgroup> line_data: array<atomic<u32>, 64>;
var<workgroup> line_nz: array<atomic<u32>, 64>;
var<workgroup> keep_nz: array<atomic<u32>, 64>;
var<workgroup> pk_sig: array<u32, 16>;
var<workgroup> pk_gcli: array<u32, 16>;
var<workgroup> pk_data: array<u32, 16>;
var<workgroup> pk_sign: array<u32, 16>;
var<workgroup> pk_gbase: array<u32, 16>;
var<workgroup> pk_dbase: array<u32, 16>;
var<workgroup> pk_sbase: array<u32, 16>;
// Decode positions (bytes, bits) of the current packet and precinct.
var<workgroup> ctl: array<u32, 16>;
var<workgroup> werr: atomic<u32>;
var<workgroup> wprobe: u32;
var<workgroup> wsize: u32;
var<workgroup> wtotal: vec4<u32>;

const C_POS: u32 = 0u;
const C_END: u32 = 1u;
const C_SIGBIT: u32 = 2u;
const C_GSTART: u32 = 3u;
const C_GEND: u32 = 4u;
const C_DSTART: u32 = 5u;
const C_DLEN: u32 = 6u;
const C_SSTART: u32 = 7u;
const C_SLEN: u32 = 8u;
const C_OPOS: u32 = 9u;

const DONE: u32 = 0xffffffffu;

fn prm(i: u32) -> u32 {
    return tab[fT + i];
}

fn ln(l: u32, f: u32) -> u32 {
    return tab[fT + prm(P_LINES) + l * LINE_WORDS + f];
}

fn fail(code: u32) {
    atomicMax(&werr, code);
}

fn byte_at(i: u32) -> u32 {
    if (i >= prm(P_CSLEN)) {
        return 0u;
    }
    return (cs[fC + (i >> 2u)] >> ((i & 3u) * 8u)) & 0xffu;
}

// `n` (1 to 32) bits MSB first from bit `pos` of the codestream.
fn bits(pos: u32, n: u32) -> u32 {
    var v = 0u;
    var p = pos;
    var left = n;
    loop {
        if (left == 0u) {
            break;
        }
        let off = p & 7u;
        let take = min(8u - off, left);
        let x = (byte_at(p >> 3u) >> (8u - off - take)) & ((1u << take) - 1u);
        v = (v << take) | x;
        p += take;
        left -= take;
    }
    return v;
}

fn compute_gtli(gain: u32, priority: u32, q: i32, r: i32) -> i32 {
    let t = q - i32(gain) - select(0, 1, i32(priority) < r);
    return clamp(t, 0, TMAX);
}

fn bit_length(v: u32) -> u32 {
    return 32u - countLeadingZeros(v);
}

fn dequant(m: u32, gcli: i32, gtli: i32) -> u32 {
    if (gtli == 0 || gcli <= gtli || m == 0u) {
        return m;
    }
    let scale = u32(gcli - gtli + 1);
    var acc = 0u;
    var rest = m;
    loop {
        if (rest == 0u) {
            break;
        }
        acc = acc + rest;
        rest = rest >> scale;
    }
    return acc;
}

// (((d << scale) - d + (1 << g)) >> (g + 1)) << t, the u64 arithmetic of the CPU in two words.
fn quant(d: u32, g: i32, t: i32) -> u32 {
    if (g <= t) {
        return 0u;
    }
    let scale = u32(g - t + 1);
    var lo = d << scale;
    var hi = d >> (32u - scale);
    let lo1 = lo - d;
    hi = hi - select(0u, 1u, lo < d);
    let lo2 = lo1 + (1u << u32(g));
    hi = hi + select(0u, 1u, lo2 < lo1);
    let k = u32(g + 1);
    let v = (lo2 >> k) | (hi << (32u - k));
    return v << u32(t);
}

fn vertical_code(g: i32, m: i32, t: i32) -> u32 {
    let room = max(m - t, 0);
    let delta = g - m;
    if (delta > room) {
        return u32(delta + room);
    } else if (delta >= 0) {
        return u32(2 * delta);
    }
    return u32(-2 * delta - 1);
}

fn put_bits(base: u32, pos: u32, v: u32, n: u32) {
    var p = pos;
    var left = n;
    loop {
        if (left == 0u) {
            break;
        }
        let off = p & 7u;
        let take = min(8u - off, left);
        let x = (v >> (left - take)) & ((1u << take) - 1u);
        let b = x << (8u - off - take);
        let bi = p >> 3u;
        if (b != 0u) {
            if ((bi >> 2u) < prm(P_CAP)) {
                atomicOr(&outw[base + (bi >> 2u)], b << ((bi & 3u) * 8u));
            } else {
                fail(20u);
            }
        }
        p += take;
        left -= take;
    }
}

fn put_byte(base: u32, at: u32, v: u32) {
    put_bits(base, at * 8u, v & 0xffu, 8u);
}

// Exclusive prefix sum over the workgroup; the total goes to `wtotal`.
fn scan(lid: u32, v: vec4<u32>) -> vec4<u32> {
    wscan[lid] = v;
    workgroupBarrier();
    var off = 1u;
    loop {
        if (off >= WG) {
            break;
        }
        var add = vec4<u32>(0u);
        if (lid >= off) {
            add = wscan[lid - off];
        }
        workgroupBarrier();
        wscan[lid] = wscan[lid] + add;
        workgroupBarrier();
        off = off << 1u;
    }
    let incl = wscan[lid];
    if (lid == WG - 1u) {
        wtotal = incl;
    }
    workgroupBarrier();
    return incl - v;
}

fn mul_div(a: u32, b: u32, c: u32) -> u32 {
    // floor(a * b / c) for c > 0 and a result below 2^32.
    let al = a & 0xffffu;
    let ah = a >> 16u;
    let bl = b & 0xffffu;
    let bh = b >> 16u;
    let ll = al * bl;
    let lh = al * bh;
    let hl = ah * bl;
    let hh = ah * bh;
    let mid = (ll >> 16u) + (lh & 0xffffu) + (hl & 0xffffu);
    let lo = (ll & 0xffffu) | (mid << 16u);
    let hi = hh + (lh >> 16u) + (hl >> 16u) + (mid >> 16u);
    var r = 0u;
    var q = 0u;
    for (var i = 0u; i < 64u; i++) {
        let bit = select((lo >> (63u - i)) & 1u, (hi >> (31u - i)) & 1u, i < 32u);
        let top = r >> 31u;
        r = (r << 1u) | bit;
        q = q << 1u;
        if (top != 0u || r >= c) {
            r = r - c;
            q = q | 1u;
        }
    }
    return q;
}

// Scratch layout per slice.
fn s_mag(sb: u32, g: u32) -> u32 { return sb + 4u * g; }
fn s_deq(sb: u32, g: u32) -> u32 { return sb + 4u * prm(P_G) + 4u * g; }
fn s_info(sb: u32, g: u32) -> u32 { return sb + 8u * prm(P_G) + g; }
fn s_cand(sb: u32, g: u32) -> u32 { return sb + 9u * prm(P_G) + g; }
fn s_dec(sb: u32, i: u32) -> u32 { return sb + 10u * prm(P_G) + i; }
fn s_enc(sb: u32, i: u32) -> u32 { return sb + 10u * prm(P_G) + prm(P_SG) + i; }
fn s_z(sb: u32, i: u32) -> u32 { return sb + 10u * prm(P_G) + 2u * prm(P_SG) + i; }
fn z_cap() -> u32 { return prm(P_G) + 64u; }

// The prediction source of line `l` for candidates: the earlier line of its band in this
// precinct, else the slice's encoder state. Returns (gcli, gtli, present).
fn cand_top(sb: u32, l: u32, gl: u32) -> vec3<i32> {
    let prev = ln(l, 8u);
    if (prev != 0u) {
        let pl = prev - 1u;
        return vec3<i32>(i32(scr[s_cand(sb, ln(pl, 2u) + gl)]), line_t1[pl], 1);
    }
    let b = ln(l, 0u);
    if (enc_valid[b] != 0u) {
        return vec3<i32>(i32(scr[s_enc(sb, ln(l, 10u) + gl)]), enc_gtli[b], 1);
    }
    return vec3<i32>(0, 0, 0);
}

// Candidate GCLIs, coding modes and exact size of the current precinct at rank `rank`
// (camera rank `rank0`); the size goes to `wsize`. With `emit`, also writes it at byte
// `ctl[C_OPOS]` of the slice's output.
fn measure(lid: u32, sb: u32, rank: u32, rank0: u32, emit: bool, ob: u32) {
    let nb = prm(P_NB);
    let nl = prm(P_NLINES);
    let G = prm(P_G);
    let chunk = (G + WG - 1u) / WG;
    if (lid == 0u) {
        var q = i32(ctl[10]);
        var r = i32(ctl[11]);
        if (rank > rank0) {
            q = i32(rank / nb);
            r = i32(nb - rank % nb);
        }
        ctl[12] = u32(q);
        ctl[13] = u32(r);
        for (var l = 0u; l < nl; l++) {
            line_t1[l] = select(line_t0[l], compute_gtli(ln(l, 6u), ln(l, 7u), q, r), rank > rank0);
            atomicStore(&line_data[l], 0u);
            atomicStore(&line_nz[l], 0u);
            for (var m = 0u; m < 4u; m++) {
                atomicStore(&line_cost[l * 4u + m], 0u);
            }
        }
    }
    workgroupBarrier();
    // Candidate GCLIs.
    let g0 = min(G, lid * chunk);
    let g1 = min(G, g0 + chunk);
    for (var g = g0; g < g1; g++) {
        let l = tab[fT + prm(P_G2L) + g];
        let t0 = line_t0[l];
        let t1 = line_t1[l];
        let info = scr[s_info(sb, g)];
        var cand = 0u;
        var nz = 0u;
        if (t1 == t0) {
            cand = (info >> 16u) & 0xffu;
        } else {
            let gnew = i32((info >> 8u) & 0xffu);
            var any = 0u;
            if (gnew > t1) {
                for (var j = 0u; j < 4u; j++) {
                    let m = quant(scr[s_deq(sb, g) + j], gnew, t1);
                    any |= m;
                    nz += select(0u, 1u, m != 0u);
                }
            }
            cand = select(u32(t1), u32(gnew), any != 0u);
            atomicAdd(&line_nz[l], nz);
        }
        scr[s_cand(sb, g)] = cand;
        let planes = i32(cand) - t1;
        if (planes > 0) {
            atomicAdd(&line_data[l], u32(planes) * 4u);
        }
    }
    storageBarrier();
    workgroupBarrier();
    if (lid == 0u) {
        for (var l = 0u; l < nl; l++) {
            if (line_t1[l] == line_t0[l]) {
                atomicStore(&line_nz[l], atomicLoad(&keep_nz[l]));
            }
        }
    }
    // Mode costs per significance group (see tico::accumulate_cost).
    let nsg = prm(P_NSG);
    let sc = (nsg + WG - 1u) / WG;
    let s0 = min(nsg, lid * sc);
    let s1 = min(nsg, s0 + sc);
    for (var sg = s0; sg < s1; sg++) {
        let l = tab[fT + prm(P_SG2L) + sg];
        let loc = sg - ln(l, 11u);
        let goff = ln(l, 2u);
        let groups = ln(l, 3u);
        let t = line_t1[l];
        let a = loc * SIG_GROUP;
        let e = min(groups, a + SIG_GROUP);
        var bz = 0u;
        var bv = 0u;
        var any_z = 0u;
        var any_v = 0u;
        var have_top = false;
        for (var gl = a; gl < e; gl++) {
            let gc = i32(scr[s_cand(sb, goff + gl)]);
            let xz = u32(gc - t);
            bz += xz + 1u;
            any_z |= xz;
            let tp = cand_top(sb, l, gl);
            if (tp.z != 0) {
                have_top = true;
                let floor = max(tp.y, t);
                let xv = vertical_code(gc, max(tp.x, floor), t);
                bv += xv + 1u;
                any_v |= xv;
            }
        }
        if (!have_top) {
            bv = bz;
            any_v = any_z;
        }
        atomicAdd(&line_cost[l * 4u + 0u], bz);
        atomicAdd(&line_cost[l * 4u + 1u], bv);
        atomicAdd(&line_cost[l * 4u + 2u], 1u + select(0u, bz, any_z != 0u));
        atomicAdd(&line_cost[l * 4u + 3u], 1u + select(0u, bv, any_v != 0u));
        if (sg < z_cap()) {
            scr[s_z(sb, sg)] = select(0u, 1u, any_z != 0u) | select(0u, 2u, any_v != 0u);
        }
    }
    storageBarrier();
    workgroupBarrier();
    if (lid == 0u) {
        for (var b = 0u; b < nb; b++) {
            var c = array<u32, 4>(0u, 0u, 0u, 0u);
            for (var l = 0u; l < nl; l++) {
                if (ln(l, 0u) == b) {
                    for (var m = 0u; m < 4u; m++) {
                        c[m] += atomicLoad(&line_cost[l * 4u + m]);
                    }
                }
            }
            var best = 3u;
            for (var m = 0u; m < 4u; m++) {
                if (c[m] < c[best]) {
                    best = m;
                }
            }
            out_mode[b] = best;
        }
        let hlen = prm(P_HLEN);
        var total = hlen;
        var pos = ctl[C_OPOS] + hlen;
        var gsum = 0u;
        var dsum = 0u;
        var ssum = 0u;
        for (var k = 0u; k < prm(P_NPK); k++) {
            let pk = prm(P_PKS) + k * 4u;
            var sig = 0u;
            var gb = 0u;
            var db = 0u;
            var sb2 = 0u;
            for (var l = tab[fT + pk]; l < tab[fT + pk + 1u]; l++) {
                let mode = out_mode[ln(l, 0u)];
                let sgs = ln(l, 5u);
                line_sigoff[l] = sig;
                var cost = atomicLoad(&line_cost[l * 4u + mode]);
                if ((mode & 2u) != 0u) {
                    sig += sgs;
                    cost -= sgs;
                }
                gb += cost;
                db += atomicLoad(&line_data[l]);
                sb2 += atomicLoad(&line_nz[l]);
            }
            let sigb = (sig + 7u) / 8u;
            let gcb = (gb + 7u) / 8u;
            let dab = (db + 7u) / 8u;
            let sgb = (sb2 + 7u) / 8u;
            if (emit) {
                if (dab >= (1u << 20u) || gcb >= (1u << 20u) || sgb >= (1u << 15u)) {
                    fail(21u);
                }
                // Packet header: data, gcli and sign lengths in 56 bits.
                put_bits(ob, pos * 8u, (dab << 3u) | (gcb >> 17u), 24u);
                put_bits(ob, pos * 8u + 24u, ((gcb & 0x1ffffu) << 15u) | sgb, 32u);
                pk_sig[k] = (pos + 7u) * 8u;
                pk_gcli[k] = (pos + 7u + sigb) * 8u;
                pk_data[k] = (pos + 7u + sigb + gcb) * 8u;
                pk_sign[k] = (pos + 7u + sigb + gcb + dab) * 8u;
                pk_gbase[k] = gsum;
                pk_dbase[k] = dsum;
                pk_sbase[k] = ssum;
            }
            gsum += gb;
            dsum += db;
            ssum += sb2;
            let plen = 7u + sigb + gcb + dab + sgb;
            pos += plen;
            total += plen;
        }
        wsize = total;
    }
    workgroupBarrier();
    if (!emit) {
        return;
    }
    // Precinct header.
    if (lid == 0u) {
        let at = ctl[C_OPOS];
        let length = wsize - prm(P_HLEN);
        if (length >= (1u << 24u)) {
            fail(22u);
        }
        put_byte(ob, at, length >> 16u);
        put_byte(ob, at + 1u, length >> 8u);
        put_byte(ob, at + 2u, length);
        put_byte(ob, at + 3u, ctl[12]);
        put_byte(ob, at + 4u, ctl[13]);
        for (var b = 0u; b < nb; b++) {
            put_bits(ob, (at + 5u + b / 4u) * 8u + 2u * (b % 4u), out_mode[b], 2u);
        }
    }
    // Significance flags.
    for (var sg = s0; sg < s1; sg++) {
        let l = tab[fT + prm(P_SG2L) + sg];
        let b = ln(l, 0u);
        let mode = out_mode[b];
        if ((mode & 2u) != 0u) {
            let flags = scr[s_z(sb, sg)];
            let vertical = (mode & 1u) != 0u && cand_top(sb, l, 0u).z != 0;
            let any = select(flags & 1u, (flags >> 1u) & 1u, vertical);
            if (any == 0u) {
                let k = ln(l, 1u);
                put_bits(ob, pk_sig[k] + line_sigoff[l] + (sg - ln(l, 11u)), 1u, 1u);
            }
        }
    }
    // Code lengths, data bits and sign bits per group, placed by prefix sums.
    var local = vec4<u32>(0u);
    for (var g = g0; g < g1; g++) {
        local += group_lengths(sb, g);
    }
    var off = scan(lid, local);
    for (var g = g0; g < g1; g++) {
        let l = tab[fT + prm(P_G2L) + g];
        let k = ln(l, 1u);
        let lens = group_lengths(sb, g);
        let b = ln(l, 0u);
        let mode = out_mode[b];
        let t = line_t1[l];
        let gl = g - ln(l, 2u);
        let cand = i32(scr[s_cand(sb, g)]);
        // GCLI code.
        if (lens.x != 0u) {
            let tp = cand_top(sb, l, gl);
            var x = u32(cand - t);
            if ((mode & 1u) != 0u && tp.z != 0) {
                x = vertical_code(cand, max(tp.x, max(tp.y, t)), t);
            }
            var p = pk_gcli[k] + off.x - pk_gbase[k];
            var ones = x;
            loop {
                if (ones == 0u) {
                    break;
                }
                let n = min(ones, 32u);
                put_bits(ob, p, 0xffffffffu, n);
                p += n;
                ones -= n;
            }
        }
        // Magnitudes of the candidate (as requant_line or tidy would leave them).
        var m = array<u32, 4>(0u, 0u, 0u, 0u);
        var neg = 0u;
        let info = scr[s_info(sb, g)];
        let t0 = line_t0[l];
        let width = ln(l, 4u);
        if (t == t0) {
            for (var j = 0u; j < 4u; j++) {
                if (gl * 4u + j < width) {
                    m[j] = scr[s_mag(sb, g) + j];
                }
            }
        } else {
            let gnew = i32((info >> 8u) & 0xffu);
            if (gnew > t) {
                for (var j = 0u; j < 4u; j++) {
                    m[j] = quant(scr[s_deq(sb, g) + j], gnew, t);
                }
            }
        }
        neg = (info >> 24u) & 15u;
        // Data: bit planes MSB first, four coefficients per plane.
        if (lens.y != 0u) {
            let planes = lens.y / 4u;
            var p = pk_data[k] + off.y - pk_dbase[k];
            var k2 = planes;
            loop {
                if (k2 == 0u) {
                    break;
                }
                let take = min(k2, 8u);
                var v = 0u;
                for (var i = 0u; i < take; i++) {
                    let bit = k2 - 1u - i;
                    let nib = (((m[0] >> u32(t)) >> bit) & 1u) << 3u | (((m[1] >> u32(t)) >> bit) & 1u) << 2u
                        | (((m[2] >> u32(t)) >> bit) & 1u) << 1u | ((m[3] >> u32(t)) >> bit) & 1u;
                    v = (v << 4u) | nib;
                }
                put_bits(ob, p, v, take * 4u);
                p += take * 4u;
                k2 -= take;
            }
        }
        // Signs of the nonzero magnitudes.
        if (lens.z != 0u) {
            var p = pk_sign[k] + off.z - pk_sbase[k];
            for (var j = 0u; j < 4u; j++) {
                if (m[j] != 0u) {
                    put_bits(ob, p, (neg >> j) & 1u, 1u);
                    p += 1u;
                }
            }
        }
        off += lens;
    }
    storageBarrier();
    workgroupBarrier();
}

// (gcli code bits, data bits, sign bits, 0) of group `g` at the candidate in `scr`.
fn group_lengths(sb: u32, g: u32) -> vec4<u32> {
    let l = tab[fT + prm(P_G2L) + g];
    let b = ln(l, 0u);
    let mode = out_mode[b];
    let t = line_t1[l];
    let gl = g - ln(l, 2u);
    let cand = i32(scr[s_cand(sb, g)]);
    var r = vec4<u32>(0u);
    let tp = cand_top(sb, l, gl);
    let vertical = (mode & 1u) != 0u && tp.z != 0;
    var x = u32(cand - t);
    if (vertical) {
        x = vertical_code(cand, max(tp.x, max(tp.y, t)), t);
    }
    var emitted = true;
    if ((mode & 2u) != 0u) {
        let sg = ln(l, 11u) + gl / SIG_GROUP;
        let flags = scr[s_z(sb, sg)];
        let any = select(flags & 1u, (flags >> 1u) & 1u, vertical);
        emitted = any != 0u;
    }
    if (emitted) {
        r.x = x + 1u;
    }
    let planes = cand - t;
    if (planes > 0) {
        r.y = u32(planes) * 4u;
    }
    // Nonzero magnitudes.
    let info = scr[s_info(sb, g)];
    let t0 = line_t0[l];
    let width = ln(l, 4u);
    var nz = 0u;
    if (t == t0) {
        for (var j = 0u; j < 4u; j++) {
            if (gl * 4u + j < width && scr[s_mag(sb, g) + j] != 0u) {
                nz += 1u;
            }
        }
    } else {
        let gnew = i32((info >> 8u) & 0xffu);
        if (gnew > t) {
            for (var j = 0u; j < 4u; j++) {
                nz += select(0u, 1u, quant(scr[s_deq(sb, g) + j], gnew, t) != 0u);
            }
        }
    }
    r.z = nz;
    return r;
}

@compute @workgroup_size(256)
fn requant(@builtin(workgroup_id) wid: vec3<u32>, @builtin(local_invocation_index) lid: u32) {
    let fb = wid.y * FRAME_WORDS;
    fT = frames[fb];
    fC = frames[fb + 1u];
    fM = frames[fb + 2u];
    fO = frames[fb + 3u];
    fR = frames[fb + 4u];
    fS = frames[fb + 5u];
    let s = wid.x;
    // Frames have different slice counts: the extra workgroups of this frame stop here.
    if (s >= frames[fb + 6u]) {
        return;
    }
    let nb = prm(P_NB);
    let nl = prm(P_NLINES);
    let G = prm(P_G);
    let P = prm(P_PRECINCTS);
    let sliceh = prm(P_SLICEH);
    let sb = fS + s * prm(P_STRIDE);
    let ob = fO + s * prm(P_CAP);
    let chunk = (G + WG - 1u) / WG;
    let g0 = min(G, lid * chunk);
    let g1 = min(G, g0 + chunk);
    let p0 = s * sliceh;
    let p1 = min(P, p0 + sliceh);

    // Rate control state, kept by invocation 0.
    var left = 0u;
    var wleft = 0u;
    var hint = prm(P_START);
    var requantized = 0u;
    var rank_sum = 0u;
    if (lid == 0u) {
        atomicStore(&werr, 0u);
        for (var b = 0u; b < 32u; b++) {
            dec_valid[b] = 0u;
            enc_valid[b] = 0u;
            dec_gtli[b] = 0;
            enc_gtli[b] = 0;
        }
        left = fm[fM + 3u * P + s];
        for (var p = p0; p < p1; p++) {
            wleft += fm[fM + P + p];
        }
        ctl[C_OPOS] = 6u;
        put_byte(ob, 0u, 0xffu);
        put_byte(ob, 1u, 0x20u);
        put_byte(ob, 2u, 0x00u);
        put_byte(ob, 3u, 0x04u);
        put_byte(ob, 4u, s >> 8u);
        put_byte(ob, 5u, s);
    }
    workgroupBarrier();

    for (var p = p0; p < p1; p++) {
        // ---- Decode the precinct (tico::decode_precinct with magnitudes).
        if (lid == 0u) {
            let off = fm[fM + p];
            let hlen = prm(P_HLEN);
            let length = (byte_at(off) << 16u) | (byte_at(off + 1u) << 8u) | byte_at(off + 2u);
            let q = byte_at(off + 3u);
            let r = byte_at(off + 4u);
            ctl[10] = q;
            ctl[11] = r;
            for (var b = 0u; b < nb; b++) {
                cam_mode[b] = (byte_at(off + 5u + b / 4u) >> (6u - 2u * (b % 4u))) & 3u;
            }
            ctl[C_POS] = off + hlen;
            ctl[C_END] = off + hlen + length;
            if (off + hlen + length > prm(P_CSLEN)) {
                fail(1u);
            }
            for (var l = 0u; l < nl; l++) {
                line_t0[l] = compute_gtli(ln(l, 6u), ln(l, 7u), i32(q), i32(r));
                atomicStore(&keep_nz[l], 0u);
            }
        }
        workgroupBarrier();
        for (var k = 0u; k < prm(P_NPK); k++) {
            let pk = prm(P_PKS) + k * 4u;
            let pg0 = tab[fT + pk + 2u];
            let png = tab[fT + pk + 3u];
            if (lid == 0u) {
                let pos = ctl[C_POS];
                let hi = bits(pos * 8u, 24u);
                let lo = bits(pos * 8u + 24u, 32u);
                if ((hi >> 23u) != 0u) {
                    // Raw GCLI packets: left to the CPU.
                    fail(2u);
                }
                let data_len = (hi >> 3u) & 0xfffffu;
                let gcli_len = ((hi & 7u) << 17u) | (lo >> 15u);
                let sign_len = lo & 0x7fffu;
                var sig = 0u;
                for (var l = tab[fT + pk]; l < tab[fT + pk + 1u]; l++) {
                    line_sigoff[l] = sig;
                    if ((cam_mode[ln(l, 0u)] & 2u) != 0u) {
                        sig += ln(l, 5u);
                    }
                }
                ctl[C_SIGBIT] = (pos + 7u) * 8u;
                let gstart = pos + 7u + (sig + 7u) / 8u;
                ctl[C_GSTART] = gstart;
                ctl[C_GEND] = gstart + gcli_len;
                ctl[C_DSTART] = gstart + gcli_len;
                ctl[C_DLEN] = data_len;
                ctl[C_SSTART] = gstart + gcli_len + data_len;
                ctl[C_SLEN] = sign_len;
                ctl[C_POS] = gstart + gcli_len + data_len + sign_len;
                if (ctl[C_POS] > ctl[C_END]) {
                    fail(3u);
                }
            }
            workgroupBarrier();
            let pc = (png + WG - 1u) / WG;
            let a0 = pg0 + min(png, lid * pc);
            let a1 = pg0 + min(png, lid * pc + pc);
            let gstart = ctl[C_GSTART];
            let gbytes = ctl[C_GEND] - gstart;
            let bc = (gbytes + WG - 1u) / WG;
            let b0 = min(gbytes, lid * bc);
            let b1 = min(gbytes, b0 + bc);
            // Which groups have a code, and where the zero bits that end codes are.
            var cnt = vec4<u32>(0u);
            for (var g = a0; g < a1; g++) {
                cnt.x += has_code(g);
            }
            for (var i = b0; i < b1; i++) {
                cnt.y += 8u - countOneBits(byte_at(gstart + i));
            }
            let base = scan(lid, cnt);
            let zeros = wtotal.y;
            var ci = base.x;
            for (var g = a0; g < a1; g++) {
                if (has_code(g) != 0u) {
                    scr[s_cand(sb, g)] = ci;
                    ci += 1u;
                } else {
                    scr[s_cand(sb, g)] = DONE;
                }
            }
            var zi = base.y;
            for (var i = b0; i < b1; i++) {
                let v = byte_at(gstart + i);
                for (var j = 0u; j < 8u; j++) {
                    if (((v >> (7u - j)) & 1u) == 0u) {
                        if (zi < z_cap()) {
                            scr[s_z(sb, zi)] = (gstart + i) * 8u + j;
                        }
                        zi += 1u;
                    }
                }
            }
            storageBarrier();
            workgroupBarrier();
            // GCLIs.
            var dl = vec4<u32>(0u);
            for (var g = a0; g < a1; g++) {
                let l = tab[fT + prm(P_G2L) + g];
                let b = ln(l, 0u);
                let t = line_t0[l];
                let gl = g - ln(l, 2u);
                let c = scr[s_cand(sb, g)];
                var x = 0u;
                if (c != DONE) {
                    if (c >= zeros || c >= z_cap()) {
                        fail(4u);
                    } else {
                        let end = scr[s_z(sb, c)];
                        var st = gstart * 8u;
                        if (c > 0u) {
                            st = scr[s_z(sb, c - 1u)] + 1u;
                        }
                        x = end - st;
                        if (x >= 32u) {
                            fail(5u);
                            x = 0u;
                        }
                    }
                }
                var v = 0;
                if (dec_valid[b] != 0u && (cam_mode[b] & 1u) != 0u) {
                    let top = i32(scr[s_dec(sb, ln(l, 10u) + gl)]);
                    let floor = max(dec_gtli[b], t);
                    let m = max(top, floor);
                    if (c == DONE) {
                        v = m;
                    } else {
                        let xi = i32(x);
                        let room = max(m - t, 0);
                        var delta = 0;
                        if (xi > 2 * room) {
                            delta = xi - room;
                        } else if ((xi & 1) != 0) {
                            delta = -(xi + 1) / 2;
                        } else {
                            delta = xi / 2;
                        }
                        v = m + delta;
                    }
                    if (v < 0 || v > TMAX) {
                        fail(6u);
                        v = t;
                    }
                } else {
                    v = t;
                    if (c != DONE) {
                        v = t + i32(x);
                    }
                    if (v > TMAX) {
                        fail(6u);
                        v = t;
                    }
                }
                scr[s_info(sb, g)] = u32(v);
                let planes = v - t;
                if (planes > 0) {
                    dl.x += u32(planes) * 4u;
                }
            }
            let dbase = scan(lid, dl);
            if (lid == 0u && wtotal.x > ctl[C_DLEN] * 8u) {
                fail(7u);
            }
            // Magnitudes.
            var nzl = vec4<u32>(0u);
            var dp = ctl[C_DSTART] * 8u + dbase.x;
            for (var g = a0; g < a1; g++) {
                let l = tab[fT + prm(P_G2L) + g];
                let t = line_t0[l];
                let v = i32(scr[s_info(sb, g)]);
                var m = array<u32, 4>(0u, 0u, 0u, 0u);
                let planes = v - t;
                if (planes > 0) {
                    var kk = u32(planes);
                    loop {
                        if (kk == 0u) {
                            break;
                        }
                        let take = min(kk, 8u);
                        let w = bits(dp, take * 4u);
                        for (var i = 0u; i < take; i++) {
                            let nib = (w >> (4u * (take - 1u - i))) & 15u;
                            let bit = kk - 1u - i;
                            m[0] |= ((nib >> 3u) & 1u) << bit;
                            m[1] |= ((nib >> 2u) & 1u) << bit;
                            m[2] |= ((nib >> 1u) & 1u) << bit;
                            m[3] |= (nib & 1u) << bit;
                        }
                        dp += take * 4u;
                        kk -= take;
                    }
                }
                for (var j = 0u; j < 4u; j++) {
                    m[j] = m[j] << u32(t);
                    scr[s_mag(sb, g) + j] = m[j];
                    nzl.x += select(0u, 1u, m[j] != 0u);
                }
            }
            let sbase = scan(lid, nzl);
            if (lid == 0u && wtotal.x > ctl[C_SLEN] * 8u) {
                fail(8u);
            }
            var sp = ctl[C_SSTART] * 8u + sbase.x;
            for (var g = a0; g < a1; g++) {
                var neg = 0u;
                for (var j = 0u; j < 4u; j++) {
                    if (scr[s_mag(sb, g) + j] != 0u) {
                        neg |= bits(sp, 1u) << j;
                        sp += 1u;
                    }
                }
                scr[s_info(sb, g)] = scr[s_info(sb, g)] | (neg << 24u);
            }
            storageBarrier();
            workgroupBarrier();
            // Prediction state after the packet.
            for (var g = a0; g < a1; g++) {
                let l = tab[fT + prm(P_G2L) + g];
                scr[s_dec(sb, ln(l, 10u) + g - ln(l, 2u))] = scr[s_info(sb, g)] & 0xffu;
            }
            if (lid == 0u) {
                for (var l = tab[fT + pk]; l < tab[fT + pk + 1u]; l++) {
                    dec_gtli[ln(l, 0u)] = line_t0[l];
                    dec_valid[ln(l, 0u)] = 1u;
                }
            }
            storageBarrier();
            workgroupBarrier();
        }

        // ---- Everything a candidate needs that does not depend on its rank.
        for (var g = g0; g < g1; g++) {
            let l = tab[fT + prm(P_G2L) + g];
            let t0 = line_t0[l];
            let width = ln(l, 4u);
            let gl = g - ln(l, 2u);
            let info = scr[s_info(sb, g)];
            let gc = i32(info & 0xffu);
            var mx = 0u;
            var any = 0u;
            var nz = 0u;
            for (var j = 0u; j < 4u; j++) {
                var r = 0u;
                if (gl * 4u + j < width) {
                    let m = scr[s_mag(sb, g) + j];
                    r = dequant(m, gc, t0);
                    any |= m;
                    nz += select(0u, 1u, m != 0u);
                }
                scr[s_deq(sb, g) + j] = r;
                mx = max(mx, r);
            }
            let keep = select(u32(gc), u32(t0), any == 0u);
            scr[s_info(sb, g)] = (info & 0xff0000ffu) | (bit_length(mx) << 8u) | (keep << 16u);
            atomicAdd(&keep_nz[l], nz);
        }
        storageBarrier();
        workgroupBarrier();

        // ---- Rank search (rate::finest_fitting), decided by invocation 0.
        var rank0 = 0u;
        var lo = 0u;
        var hi = 0u;
        var share = 0u;
        var slack = 0u;
        var phase = 0u;
        var good = 0u;
        var good_size = 0u;
        var bad = 0u;
        var last = 0u;
        var step = 1u;
        var best = 0u;
        if (lid == 0u) {
            let q = ctl[10];
            let r = ctl[11];
            rank0 = q * nb + (nb - min(r, nb));
            lo = rank0;
            hi = max(prm(P_HI), rank0);
            let w = fm[fM + P + p];
            if (wleft == 0u) {
                share = left;
            } else {
                share = mul_div(left, w, wleft);
            }
            var guess = hint;
            if (prm(P_SEEDED) != 0u) {
                guess = fm[fM + 2u * P + p];
                slack = share / 32u;
            }
            guess = max(guess, rank0);
            wprobe = clamp(guess, lo, hi);
        }
        loop {
            let probe = workgroupUniformLoad(&wprobe);
            if (probe == DONE) {
                break;
            }
            measure(lid, sb, probe, ctl[10] * nb + (nb - min(ctl[11], nb)), false, ob);
            if (lid == 0u) {
                let size = wsize;
                let fits = size <= share;
                var next = DONE;
                if (phase == 0u) {
                    if (fits && share - size <= slack) {
                        best = probe;
                    } else if (fits) {
                        good = probe;
                        good_size = size;
                        step = 1u;
                        phase = 1u;
                        if (good == lo) {
                            best = good;
                        } else {
                            next = finer(good, step, lo);
                        }
                    } else {
                        bad = probe;
                        last = probe;
                        step = 1u;
                        phase = 2u;
                        if (bad == hi) {
                            best = last;
                        } else {
                            next = min(bad + step, hi);
                        }
                    }
                } else if (phase == 1u) {
                    if (fits) {
                        good = probe;
                        step *= 2u;
                        if (good == lo) {
                            best = good;
                        } else {
                            next = finer(good, step, lo);
                        }
                    } else {
                        bad = probe;
                        phase = 3u;
                    }
                } else if (phase == 2u) {
                    if (fits) {
                        good = probe;
                        phase = 3u;
                    } else {
                        bad = probe;
                        last = probe;
                        step *= 2u;
                        if (bad == hi) {
                            best = last;
                        } else {
                            next = min(bad + step, hi);
                        }
                    }
                } else {
                    if (fits) {
                        good = probe;
                    } else {
                        bad = probe;
                    }
                }
                if (phase == 3u && next == DONE) {
                    if (bad + 1u < good) {
                        next = bad + (good - bad) / 2u;
                    } else {
                        best = good;
                    }
                }
                wprobe = next;
            }
            workgroupBarrier();
        }
        // ---- Code the chosen rank.
        if (lid == 0u) {
            wprobe = best;
        }
        let chosen = workgroupUniformLoad(&wprobe);
        let r0 = ctl[10] * nb + (nb - min(ctl[11], nb));
        measure(lid, sb, chosen, r0, true, ob);
        // Encoder state after the precinct: each band's last line.
        for (var g = g0; g < g1; g++) {
            let l = tab[fT + prm(P_G2L) + g];
            if (ln(l, 9u) != 0u) {
                scr[s_enc(sb, ln(l, 10u) + g - ln(l, 2u))] = scr[s_cand(sb, g)];
            }
        }
        if (lid == 0u) {
            for (var l = 0u; l < nl; l++) {
                enc_gtli[ln(l, 0u)] = line_t1[l];
                enc_valid[ln(l, 0u)] = 1u;
            }
            res[fR + 4u * prm(P_SLICES) + p] = chosen;
            if (chosen > rank0) {
                requantized += 1u;
            }
            rank_sum += rank0;
            hint = chosen;
            left = left - min(left, wsize);
            wleft = wleft - min(wleft, fm[fM + P + p]);
            ctl[C_OPOS] = ctl[C_OPOS] + wsize;
            if (ctl[C_OPOS] > prm(P_CAP) * 4u) {
                fail(23u);
            }
        }
        storageBarrier();
        workgroupBarrier();
    }
    if (lid == 0u) {
        res[fR + 4u * s] = ctl[C_OPOS];
        res[fR + 4u * s + 1u] = atomicLoad(&werr);
        res[fR + 4u * s + 2u] = requantized;
        res[fR + 4u * s + 3u] = rank_sum;
    }
}

// 1 when group `g` of the packet being decoded has a GCLI code (it is not in a
// significance group flagged insignificant).
fn has_code(g: u32) -> u32 {
    let l = tab[fT + prm(P_G2L) + g];
    let b = ln(l, 0u);
    if ((cam_mode[b] & 2u) == 0u) {
        return 1u;
    }
    let sg = (g - ln(l, 2u)) / SIG_GROUP;
    let flag = bits(ctl[C_SIGBIT] + line_sigoff[l] + sg, 1u);
    return 1u - flag;
}

// max(rank - step, lo) without wrapping.
fn finer(rank: u32, step: u32, lo: u32) -> u32 {
    if (step >= rank) {
        return lo;
    }
    return max(rank - step, lo);
}
