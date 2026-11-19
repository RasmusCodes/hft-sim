# Mutation testing: plant one realistic bug at a time in a scratch copy of the workspace and
# check that `cargo test` fails. A surviving mutant means either the tests cannot see that bug,
# or the mutation cannot change behaviour (an "equivalent mutant"; docs/TESTING.md explains the
# two known ones).
#
# Usage (from anywhere):  powershell -File tools\mutation_test.ps1 [-Only lob|sim|agents|mm|metrics|data|all] [-Ids A13,S02] [-Scratch <dir>]
# About an hour for all; nothing in the repository is modified. `mm` mutants run the agents suite;
# `metrics` mutants run the analysis suite and the agents' metrics tests (ledger identity, races);
# `data` mutants run the data suite and the simulated-day tests.
param(
  [ValidateSet('all', 'lob', 'sim', 'agents', 'mm', 'metrics', 'data')][string]$Only = 'all',
  [string[]]$Ids = @(),
  [string]$Scratch = (Join-Path $env:TEMP 'hftsim-mutants')
)

$ErrorActionPreference = 'Continue'
# `powershell -File` passes "-Ids A13,S02" as one string, so split it here.
$Ids = @($Ids | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() } | Where-Object { $_ })
$src = Split-Path -Parent $PSScriptRoot
$ws  = Join-Path $Scratch 'ws'
New-Item -ItemType Directory -Force (Join-Path $ws 'crates') | Out-Null
$env:CARGO_TARGET_DIR = Join-Path $Scratch 'target'

$E = 'crates\lob\src\engine.rs'; $A = 'crates\lob\src\auction.rs'; $B = 'crates\lob\src\book.rs'; $M = 'crates\lob\src\mbo.rs'
$L = 'crates\sim\src\latency.rs'; $K = 'crates\sim\src\simulation.rs'; $G = 'crates\sim\src\gateway.rs'
$Q = 'crates\sim\src\queue.rs'; $AG = 'crates\sim\src\agent.rs'
$AC = 'crates\agents\src\common.rs'; $AI = 'crates\agents\src\informed.rs'; $AF = 'crates\agents\src\fundamental.rs'
$AE = 'crates\agents\src\execution.rs'; $AL = 'crates\agents\src\latency_arb.rs'; $AP = 'crates\agents\src\pairs.rs'
$AN = 'crates\agents\src\noise.rs'; $AY = 'crates\analysis\src\lib.rs'
$BM = 'crates\agents\src\mm\models.rs'; $BE = 'crates\agents\src\mm\estimators.rs'; $BS = 'crates\agents\src\mm\strategy.rs'
$BA = 'crates\agents\src\mm\agent.rs'; $MT = 'crates\analysis\src\metrics.rs'
$DL = 'crates\data\src\lobster.rs'; $DR = 'crates\data\src\replay.rs'; $D3 = 'crates\data\src\l3.rs'; $DI = 'crates\data\src\itch.rs'
# id, package (whose tests run), file, original text, mutated text, description
$mutants = @(
  @('M01', 'lob', $E, 'if price == old_price && new_leaves <= old_leaves {', 'if price == old_price && new_leaves < old_leaves {', 'modify to the same size loses priority'),
  @('M02', 'lob', $E, 'Side::Buy => resting <= limit,', 'Side::Buy => resting < limit,', 'buy limit does not match at an equal price'),
  @('M03', 'lob', $E, 'if stp != StpMode::CancelResting {', 'if stp == StpMode::CancelBoth {', 'FOK feasibility ignores cancel-aggressor STP'),
  @('M04', 'lob', $A, 'd < dp || (d == dp && c > p)', 'd < dp || (d == dp && c < p)', 'auction reference tie goes to the lower price (equivalent)'),
  @('M05', 'lob', $A, 'prices[k + 1] - prices[k] >= 2 * tick', 'prices[k + 1] - prices[k] > 2 * tick', 'auction skips one-tick gaps between limits'),
  @('M06', 'lob', $A, '(s.hi, *s)', '(s.lo, *s)', 'buy-surplus auction picks the bottom of the top interval (equivalent)'),
  @('M07', 'lob', $E, 'if t.stp == StpMode::CancelBoth {', 'if t.stp == StpMode::CancelResting {', 'STP cancel-both keeps the resting order'),
  @('M08', 'lob', $E, 'Side::Buy => self.best_ask().is_some_and(|(a, _)| price >= a),', 'Side::Buy => self.best_ask().is_some_and(|(a, _)| price > a),', 'post-only buy at the ask is accepted'),
  @('M09', 'lob', $E, 'if qty <= cum {', 'if qty < cum {', 'modify to exactly the filled qty is not treated as done'),
  @('M10', 'lob', $B, 'levels.pop();', '{}', 'emptied best level is not removed'),
  @('M11', 'lob', $E, 'victims.sort_unstable_by_key(|&(id, _)| id);', '', 'mass cancel order follows hash iteration'),
  @('M12', 'lob', $A, 'while b > 0 && bids[b - 1].0 >= prices[k] {', 'while b > 0 && bids[b - 1].0 > prices[k] {', 'auction buy demand excludes bids at the price'),
  @('M13', 'lob', $E, 'n.cum_qty += t.filled;', 'n.cum_qty += 0;', 'aggressive modify forgets its fills'),
  @('M14', 'lob', $E, 'Side::Buy => lvl.price >= price,', 'Side::Buy => lvl.price > price,', 'uncross excludes bids priced at the uncross price'),
  @('M15', 'lob', $M, 'Some(Side::Buy) => self.execute(sell_order_id, qty)?,', 'Some(Side::Buy) => self.execute(buy_order_id, qty)?,', 'feed handler reduces the wrong order'),
  @('M16', 'lob', $E, 'self.reference_price = level_price;', '', 'continuous trades do not update the reference price'),
  @('M17', 'lob', $E, 'if !crosses(side, lim, lvl.price) {', 'if false && !crosses(side, lim, lvl.price) {', 'FOK feasibility ignores the limit price'),
  @('M18', 'lob', $E, 'self.reference_price = q.price;', '', 'auctions do not update the reference price'),
  @('S01', 'sim', $L, 'let arrival = sent.saturating_add(self.model.sample(&mut self.rng)).max(self.last_arrival);', 'let arrival = sent.saturating_add(self.model.sample(&mut self.rng));', 'links may reorder messages'),
  @('S02', 'sim', $K, 'let done = arrive.max(session.free_at).saturating_add(self.timing.gateway_ns);', 'let done = arrive.saturating_add(self.timing.gateway_ns);', 'gateway session does not queue'),
  @('S03', 'sim', $G, 'next_ids[i] += 1;', 'next_ids[i] += 2;', 'gateway predicts the wrong exchange ids'),
  @('S04', 'sim', $K, 'let service = self.timing.engine_base_ns + self.timing.engine_per_event_ns * v.out.len() as i64;', 'let service = self.timing.engine_base_ns;', 'engine service ignores work done'),
  @('S05', 'sim', $K, 'let depart = now.saturating_add(st.compute.sample(&mut st.compute_rng));', 'let depart = now;', 'agents think instantly'),
  @('S06', 'sim', $G, 'if !bucket.take(now) {', 'if false && !bucket.take(now) {', 'throttle never applies'),
  @('S07', 'sim', $G, 'Side::Buy => pos + i128::from(ex.open_buy) + extra > max,', 'Side::Buy => pos + extra > max,', 'position limit ignores open buy orders'),
  @('S08', 'sim', $Q, 'other.time.cmp(&self.time).then_with(|| other.seq.cmp(&self.seq))', 'other.time.cmp(&self.time).then_with(|| self.seq.cmp(&other.seq))', 'same-time events run last-in first-out'),
  @('S09', 'sim', $K, 'if let Err(err) = f.book.apply(e) {', 'if let Err(err) = Ok::<(), lob::MboError>(()) {', 'agents'' book views never update'),
  @('S10', 'sim', $K, 'lo + Rng::stream(seed, stream(S_SESSION, k, 0)).range_i64(0, hi - lo)', 'lo', 'random auction end is not random'),
  @('S11', 'sim', $AG, '*p += qty;', '*p += 0;', 'agent view misses its buys'),
  @('S12', 'sim', $G, 'Some(o) if o.agent == self.agent && o.order_id == Some(order_id) => o.client_id,', 'Some(o) if false => o.client_id,', 'late cancel replies cannot be routed'),
  @('S13', 'sim', $K, 'if mode == FeedMode::Callback {', 'if true {', 'snapshot members still get per-packet callbacks'),
  @('A01', 'agents', $AC, 'Side::Buy => max - (position + open_buy),', 'Side::Buy => max - position,', 'strategy position limit ignores open buy orders'),
  @('A02', 'agents', $AC, 'let max = i128::from(self.max_position).min(capital_position);', 'let max = i128::from(self.max_position);', 'capital limit ignored'),
  @('A03', 'agents', $AC, 'q / spec.lot_size * spec.lot_size', 'q', 'order room not rounded down to the lot'),
  @('A04', 'agents', $AI, 'if estimate - ask as f64 >= edge =>', 'if estimate - ask as f64 >= 0.0 =>', 'informed trader ignores its threshold when buying'),
  @('A05', 'agents', $AF, 'let price = bid.map_or(cap - tick, |b| b.min(cap));', 'let price = bid.map_or(cap - tick, |b| b);', 'fundamental trader may bid above its value estimate'),
  @('A06', 'agents', $AE, '1.0 - sinh(urgency * (1.0 - tau)) / sinh(*urgency)', 'sinh(urgency * tau) / sinh(*urgency)', 'Almgren-Chriss schedule back-loads'),
  @('A07', 'agents', $AE, '- active.start_volume;', '- 0;', 'POV counts volume traded before its start'),
  @('A08', 'agents', $AE, 'let w = profile[bucket].max(0.0);', 'let w = 1.0;', 'VWAP ignores its volume profile'),
  @('A09', 'agents', $AE, 'let deficit = target - self.reports[report].filled - in_flight;', 'let deficit = target - self.reports[report].filled;', 'execution ignores children in flight'),
  @('A10', 'agents', $AC, 'if Self::pnl(ctx, instruments) < -max_loss {', 'if Self::pnl(ctx, instruments) < -max_loss * 1_000_000 {', 'kill switch threshold a million times too loose'),
  @('A11', 'agents', $AC, 'cancel_all(ctx, i);', 'let _ = i;', 'kill switch leaves quotes resting'),
  @('A12', 'agents', $AL, 'Some((Side::Buy, ask, qty.min(clip)))', 'Some((Side::Buy, ask - 1, qty.min(clip)))', 'latency arbitrageur bids a tick below the stale ask'),
  @('A13', 'agents', $AP, 'let desired_a = -((self.beta', 'let desired_a = ((self.beta', 'pairs trader trades both legs the same way'),
  @('A14', 'agents', $AN, 'match (side, self.anchor_memory_ns.and(self.anchor.map(|x| x.0))) {', 'match (side, None::<f64>) {', 'noise traders ignore their anchor'),
  @('A15', 'agents', $AY, 'let impact_ticks = if m.side == Side::Buy { move_ticks } else { -move_ticks };', 'let impact_ticks = move_ticks;', 'metaorder impact not signed by side'),
  @('B01', 'mm', $BM, 'self.a * exp(-(1.0 + self.k / self.gamma) * ln(1.0 + self.gamma / self.k))', 'self.a * exp((1.0 + self.k / self.gamma) * ln(1.0 + self.gamma / self.k))', 'GLFT eta with the wrong exponent sign'),
  @('B02', 'mm', $BM, 'ln(vq / self.v_scaled(tau, other))', 'ln(self.v_scaled(tau, other) / vq)', 'GLFT finite-horizon quotes invert the v ratio'),
  @('B03', 'mm', $BM, 'let skew = q * self.gamma * self.sigma * self.sigma * tau;', 'let skew = -q * self.gamma * self.sigma * self.sigma * tau;', 'Avellaneda-Stoikov skew has the wrong sign'),
  @('B04', 'mm', $BM, 'let c = (self.alpha() / self.eta()).sqrt() / (2.0 * self.k);', 'let c = (self.alpha() / self.eta()).sqrt() / self.k;', 'GLFT closed form off by a factor 2'),
  @('B05', 'mm', $BE, 'e -= next.ask_qty as f64;', 'e += next.ask_qty as f64;', 'OFI counts added supply as demand'),
  @('B06', 'mm', $BE, 'Side::Buy => mid_ticks - price,', 'Side::Buy => price - mid_ticks,', 'buy mark-outs have the wrong sign'),
  @('B07', 'mm', $BE, 's * self.buckets + (self.buckets - 1 - b)', 's * self.buckets + b', 'micro-price transitions not mirrored'),
  @('B08', 'mm', $BE, 'self.sum_dt = self.sum_dt * w + dt;', 'self.sum_dt = self.sum_dt * w + self.sample_s;', 'volatility divides by the sampling step, not elapsed time'),
  @('B09', 'mm', $BE, 'if depth_ticks >= d - 1e-9 {', 'if depth_ticks <= d + 1e-9 {', 'intensity counts orders below each depth'),
  @('B10', 'mm', $BS, 'let mut room_buy = (self.max_inventory as f64 - q).floor() as i64;', 'let mut room_buy = (self.max_inventory as f64 + q).floor() as i64;', 'buy room grows with long inventory'),
  @('B11', 'mm', $BS, '(p as i64).min(best_ask - 1)', '(p as i64).min(best_ask)', 'bid may sit on the best ask (post-only reject)'),
  @('B12', 'mm', $BS, 'if bid_inside > ask_inside { (Some(a - 1), Some(a)) } else { (Some(b), Some(b + 1)) }', 'if bid_inside < ask_inside { (Some(a - 1), Some(a)) } else { (Some(b), Some(b + 1)) }', 'collision rule moves the wrong side'),
  @('B13', 'mm', $BS, 'Some(f) => base + f.ofi_ticks * s.ofi.clamp(-1.0, 1.0)', 'Some(f) => base - f.ofi_ticks * s.ofi.clamp(-1.0, 1.0)', 'order-flow skew points the wrong way'),
  @('B14', 'mm', $BS, 'if m < -t.threshold_ticks {', 'if m > -t.threshold_ticks {', 'toxicity guard widens on good mark-outs'),
  @('B15', 'mm', $BS, '(ahead as f64) <= rule.max_ahead_share * level_qty as f64', '(ahead as f64) >= rule.max_ahead_share * level_qty as f64', 'queue rule keeps orders at the back'),
  @('B16', 'mm', $BA, 'spec = spec.with_post_only();', 'spec = spec.with_tif(TimeInForce::Gtc);', 'market maker forgets post-only'),
  @('B17', 'mm', $BA, 'if !self.kill.check(ctx, &[i]) {', 'if false && !self.kill.check(ctx, &[i]) {', 'market maker ignores its kill switch'),
  @('B18', 'mm', $BA, 'crate::common::cancel_all(ctx, self.instrument);', 'let _ = &ctx;', 'stopping leaves quotes resting'),
  @('B19', 'mm', $BA, 'inventory: position as f64 / unit as f64,', 'inventory: -(position as f64) / unit as f64,', 'inventory sign flipped'),
  @('C01', 'metrics', $MT, 'eff += i128::from(d) * (2 * i128::from(price) - m2) * q;', 'eff += (2 * i128::from(price) - m2) * q;', 'effective spread not signed by side'),
  @('C02', 'metrics', $MT, 'let (b, a) = path.at(e.ts - 1);', 'let (b, a) = path.at(e.ts);', 'spreads use the mid after the execution'),
  @('C03', 'metrics', $MT, 'let mh = if e.ts + h <= to { path.mid2_at(e.ts + h).unwrap_or(final_mid2) } else { final_mid2 };', 'let mh = path.mid2_at(e.ts + h).unwrap_or(final_mid2);', 'realized spread looks past the window end'),
  @('C04', 'metrics', $MT, 'let mh = path.mid2_at((e.ts + horizon_ns).min(mark_time)).map_or(m0, i128::from);', 'let mh = path.mid2_at(e.ts + horizon_ns).map_or(m0, i128::from);', 'mark-out horizon not cut at the mark time'),
  @('C05', 'metrics', $MT, 'if from.is_some_and(|f| e.ts <= f) {', 'if from.is_some_and(|f| e.ts < f) {', 'a fill at the window start counts inside the window'),
  @('C06', 'metrics', $MT, 'out.carried = i128::from(out.opening_position) * (m_t - m_from) * scale;', 'out.carried = i128::from(out.opening_position) * (m_from - m_t) * scale;', 'carried position revalued backwards'),
  @('C07', 'metrics', $MT, 'let m0 = path.mid2_at(e.ts - 1).map_or(p2, i128::from);', 'let m0 = path.mid2_at(e.ts).map_or(p2, i128::from);', 'spread capture uses the mid after the fill'),
  @('C08', 'metrics', $MT, 'let ahead = levels[idx(side)].get(&price).copied().unwrap_or(0);', 'let ahead = levels[idx(side)].get(&price).copied().unwrap_or(0) + qty;', 'queue ahead includes the order itself'),
  @('C09', 'metrics', $MT, 'Side::Buy => levels[0].keys().next_back().is_none_or(|&b| price >= b),', 'Side::Buy => levels[0].keys().next().is_none_or(|&b| price >= b),', 'touch test compares bids with the worst bid'),
  @('C10', 'metrics', $MT, '(owners.len() >= 2, first_success, first_fail)', '(owners.len() >= 1, first_success, first_fail)', 'one participant can race itself'),
  @('C11', 'metrics', $MT, 'm.ts <= msgs[i].ts + window_ns', 'm.ts <= msgs[i].ts + 2 * window_ns', 'race window twice as long'),
  @('C12', 'metrics', $MT, 'gateway_failed_cancels.iter().filter(|g| g.0 >= from && g.0 <= to) {', 'gateway_failed_cancels.iter().filter(|g| g.0 >= from && g.0 <= to).take(0) {', 'cancels the gateway refused are ignored'),
  @('C13', 'metrics', $MT, 'e -= nqa;', 'e += nqa;', 'OFI counts new ask supply as demand'),
  @('C14', 'metrics', $MT, 'vals.push(((w[1].1 - w[0].1) as f64 / w[0].1 as f64).abs() / value);', 'vals.push(((w[1].1 - w[0].1) as f64).abs() / value);', 'Amihud uses absolute, not relative, price changes'),
  @('C15', 'metrics', $MT, '(g1 < 0.0).then(|| 2.0 * (-g1).sqrt())', '(g1 < 0.0).then(|| (-g1).sqrt())', 'Roll spread off by a factor 2'),
  @('C16', 'metrics', $MT, 'let opportunity = d * (i128::from(end2) - i128::from(arrival2)) * unfilled;', 'let opportunity = d * (i128::from(end2) - i128::from(decision2)) * unfilled;', 'opportunity cost measured from the decision price'),
  @('C17', 'metrics', $MT, 'if liquidity == Liquidity::Maker {', 'if liquidity != Liquidity::Maker {', 'maker volume counts taker fills'),
  @('C18', 'metrics', $MT, 'out.total = out.spread_capture + out.adverse_selection + out.inventory + out.carried - out.fees;', 'out.total = out.spread_capture + out.adverse_selection + out.inventory + out.carried + out.fees;', 'fees added to PnL'),
  @('C19', 'metrics', $MT, 'depth_sum += (cur.1 + cur.3) as f64 / 2.0;', 'depth_sum += (cur.1 + cur.3) as f64;', 'depth sums both sides'),
  @('C20', 'metrics', $MT, 'success: traded.contains_key(&order_id), take: true', 'success: true, take: true', 'every take counts as a success'),
  @('D01', 'data', $DL, '1 => Side::Buy,', '1 => Side::Sell,', 'LOBSTER direction 1 read as a sell order'),
  @('D02', 'data', $DL, 'secs.checked_mul(1_000_000_000)?', 'secs.checked_mul(1_000_000)?', 'seconds read as milliseconds'),
  @('D03', 'data', $DL, 's.asks.push((ap, asz));', 's.bids.push((ap, asz));', 'ask levels read into the bids'),
  @('D04', 'data', $DL, 'let cut = o.2 - qty;', 'let cut = qty;', 'partial cancellation written with the new size, not the cut'),
  @('D05', 'data', $DL, 'Some(Side::Buy) => sell_order_id,', 'Some(Side::Buy) => buy_order_id,', 'execution written for the aggressor, not the resting order'),
  @('D06', 'data', $DL, 'l.seed -= m.size.min(l.seed);', 'l.seed -= 0;', 'events on unknown orders leave the seed untouched'),
  @('D07', 'data', $DL, 'let seed = size - l.known;', 'let seed = size;', 'reconciliation seeds the whole level, counting known orders twice'),
  @('D08', 'data', $DL, 'bids: self.levels[0].iter().rev()', 'bids: self.levels[0].iter()', 'bid levels listed worst first'),
  @('D09', 'data', $DR, 'Some((m.size.min(a.left), FillReason::Queue))', 'None', 'executions behind the order never fill it'),
  @('D10', 'data', $DR, '*q -= if m.kind == Kind::Delete { *q } else { m.size.min(*q) };', '*q -= 0;', 'cancellations ahead do not move the order up'),
  @('D11', 'data', $DR, '(o.side == Side::Buy && m.price < o.price)', '(o.side == Side::Buy && m.price > o.price)', 'trade-through test for bids points the wrong way'),
  @('D12', 'data', $DR, 'Side::Buy => ask.is_some_and(|a| o.price >= a),', 'Side::Buy => ask.is_some_and(|a| o.price > a),', 'a bid at the ask is not seen as marketable'),
  @('D13', 'data', $DR, '.filter(|&(id, _)| Some(id) != o.own_order_id)', '.filter(|&(_, _)| true)', 'a leave-one-out copy queues behind its own original'),
  @('D14', 'data', $DR, 'if m.side != o.side || Some(m.order_id) == o.own_order_id {', 'if m.side != o.side {', 'a copy sees its original''s executions (fills itself with them)'),
  @('D15', 'data', $DR, 'let expire_ns = if r.cancelled { r.end_ns } else { None };', 'let expire_ns = r.end_ns;', 'copies of fully filled orders expire at the real fill'),
  @('D16', 'data', $DR, '} else if r.end_ns.is_some_and(|e| e <= r.entry_ns) {', '} else if r.end_ns.is_some_and(|e| e < r.entry_ns) {', 'orders gone within their entry stamp are compared'),
  @('D17', 'data', $DR, 'if t_model < t_real {', 'if t_model <= t_real {', 'a same-stamp first fill counted as earlier'),
  @('D18', 'data', $DR, 'Some((m.size.min(a.left), FillReason::TradeThrough))', 'Some((a.left, FillReason::TradeThrough))', 'a trade-through fills the whole rest, not what traded through'),
  @('D19', 'data', $DR, 'Side::Sell => o.side == Side::Buy && o.price >= tk.limit,', 'Side::Sell => o.side == Side::Buy,', 'a selling taker''s remainder reaches bids below its limit'),
  @('D20', 'data', $DR, 'if !reaches || o.entry_ns >= tk.ts_ns ||', 'if !reaches || o.entry_ns > tk.ts_ns ||', 'an order entered in the taker''s own stamp gets its remainder'),
  @('D21', 'data', $DR, 'Side::Buy => l.price < m.price && l.price >= limit,', 'Side::Buy => l.price < m.price,', 'the counterfactual re-matches bids beyond the taker''s limit'),
  @('D22', 'data', $DR, 'extra.push(TakerRemainder { ts_ns: ts, id: taker, side, limit, available: left });', 'let _ = (ts, taker, side, limit);', 'the counterfactual drops what stays with the taker'),
  @('L01', 'data', $D3, 'Side::Buy => (-price, id),', 'Side::Buy => (price, id),', 'L3 bids ranked worst first'),
  @('L02', 'data', $D3, 'is_none_or(|l| l.created.is_none() && l.first >= j)', 'is_none_or(|l| l.first >= j)', 'a snapshot may reveal an order the stream creates later'),
  @('L03', 'data', $D3, '!shown.contains(&id) && lives.untouched(id, retire_before))', '!shown.contains(&id) && lives.untouched(id, add_before))', 'orders the stream deletes within the window are retired by the snapshot'),
  @('L04', 'data', $D3, 'let certain = snap.ts_us.saturating_sub(stamp);', 'let certain = snap.ts_us.saturating_sub(1);', 'coarse event stamps treated as exact'),
  @('L05', 'data', $D3, 'reveal(&mut w, snap, depth, &lives, j0, horizon);', 'let _ = &lives;', 'offset search without revealing pre-recording orders'),
  @('L06', 'data', $D3, '!self.added.is_empty() || self.corrected > 0 || self.retired > 0', 'self.corrected > 0 || self.retired > 0', 'a snapshot that only reveals orders counts as exact'),
  @('L07', 'data', $D3, '    ours.next().is_none()', '    true', 'a book with more orders than the vendor lists still matches'),
  @('L08', 'data', $D3, 'usize::from(!self.held.contains(&e.id));', 'usize::from(!self.book.contains(e.id));', 'unknown-order events counted against the live book, not every order held'),
  @('L09', 'data', $D3, 'ended = events[j].ts_us;', 'ended = t;', 'offset end reported as its start'),
  @('L10', 'data', $D3, '=> -neg_bid >= ask,', '=> -neg_bid > ask,', 'a locked book is not counted as crossed'),
  @('L11', 'data', $D3, 'is_some_and(|f| id > f));', 'is_some_and(|f| id < f));', 'revealed-id check points the wrong way'),
  @('L12', 'data', $D3, 'Some(&(_, op, _)) if op != p => d.price.push((id, op, p)),', 'Some(&(_, op, _)) if op != p && false => d.price.push((id, op, p)),', 'price differences not classified'),
  @('I01', 'data', $DI, 'stock: stock(&b[24..32]), price: u32_at(32) },', 'stock: stock(&b[24..32]), price: u32_at(28) },', 'ITCH add price read 4 bytes early'),
  @('I02', 'data', $DI, 'b''E'' => 31,', 'b''E'' => 30,', 'ITCH execution length one byte short'),
  @('I03', 'data', $DI, 'books.entry(o.locate).or_default().change(o.side, o.price, -i64::from(o.shares));', 'books.entry(o.locate).or_default().change(o.side, o.price, 0);', 'a delete leaves its shares in the book'),
  @('I04', 'data', $DI, 'Some(&f) if f != oref => {', 'Some(&f) if f == oref => {', 'arrival-order check flags executions at the front'),
  @('I05', 'data', $DI, '(Some(b), Some(a)) if b >= a)', '(Some(b), Some(a)) if b > a)', 'a locked ITCH book is not counted as crossed'),
  @('I06', 'data', $DI, 'origin: o.origin, how,', 'origin: c.messages, how,', 'a replacement forgets its original order''s arrival'),
  @('I07', 'data', $DI, 'kind: Kind::ExecuteVisible, order_id: oref as i64, size: i64::from(take), price: i64::from(o.price),', 'kind: Kind::ExecuteVisible, order_id: oref as i64, size: i64::from(take), price: i64::from(px),', 'an execution with a price is exported at its printed price'),
  @('I08', 'data', $DI, 'c.locate_mismatches += u64::from(o.locate != rec.locate);', 'c.locate_mismatches += u64::from(o.locate != o.locate);', 'an execution under another stock is not flagged'),
  @('I09', 'data', $DI, 'c.out_of_order += u64::from(rec.ts_ns < last_ts);', 'c.out_of_order += u64::from(rec.ts_ns < c.first_ns);', 'timestamps compared with the first, not the latest'),
  @('I10', 'data', $DI, 'if orders.insert(oref, order).is_some() {', 'if orders.insert(oref, order).is_some() && false {', 'duplicate order references never counted')
)

$summary = @()
$prevFile = $null
$firstSync = $true
foreach ($m in $mutants) {
  $id, $pkg, $file, $old, $new, $what = $m
  if ($Only -ne 'all' -and $pkg -ne $Only) { continue }
  if ($Ids.Count -gt 0 -and $Ids -notcontains $id) { continue }
  robocopy (Join-Path $src 'crates') (Join-Path $ws 'crates') /MIR /XD target /NFL /NDL /NJH /NJS /NP | Out-Null
  Copy-Item (Join-Path $src 'Cargo.toml') (Join-Path $ws 'Cargo.toml') -Force
  # Robocopy restores files with their original timestamps, and cargo decides staleness by
  # timestamp: a restored file can look older than the build of its mutated version, and that
  # stale build would then leak into the next mutant (found 2026-11-15). So touch every source
  # once per run, and after that the file the previous mutant changed.
  if ($firstSync) {
    Get-ChildItem (Join-Path $ws 'crates') -Recurse -Filter *.rs | ForEach-Object { $_.LastWriteTime = Get-Date }
    $firstSync = $false
  } elseif ($prevFile) {
    (Get-Item (Join-Path $ws $prevFile)).LastWriteTime = Get-Date
  }
  $path = Join-Path $ws $file
  $text = [IO.File]::ReadAllText($path)
  $n = ([regex]::Matches($text, [regex]::Escape($old))).Count
  if ($n -ne 1) { $summary += "$id  SKIPPED (pattern found $n times; source changed?)  $what"; continue }
  [IO.File]::WriteAllText($path, $text.Replace($old, $new))
  $prevFile = $file
  $log = Join-Path $Scratch "$id.log"
  $suites = switch ($pkg) { 'mm' { @('-p agents') } 'metrics' { @('-p analysis', '-p agents --test metrics') } 'data' { @('-p data', '-p runner --test lobster') } default { @("-p $pkg") } }
  Set-Content $log ''
  $code = 0
  Push-Location $ws
  foreach ($suite in $suites) {
    cmd /c "cargo test --offline --no-fail-fast $suite >> `"$log`" 2>&1"
    if ($LASTEXITCODE -ne 0) { $code = $LASTEXITCODE }
  }
  Pop-Location
  $out = Get-Content $log -Raw
  if ($out -match 'could not compile') { $status = 'COMPILE-ERROR' }
  elseif ($code -ne 0) { $status = 'CAUGHT' }
  else { $status = 'SURVIVED' }
  $failing = ([regex]::Matches($out, '(?m)^test (\S+) \.\.\. FAILED')) | ForEach-Object { $_.Groups[1].Value }
  $summary += ('{0}  {1,-13} {2}  [{3} failing: {4}]' -f $id, $status, $what, @($failing).Count, (($failing | Select-Object -First 3) -join ', '))
  Write-Host $summary[-1]
}
if ($summary.Count -eq 0) { Write-Warning "No mutant matches -Only $Only -Ids $($Ids -join ',')"; exit 1 }
$summary | Out-File (Join-Path $Scratch 'summary.txt') -Encoding utf8
"`nSummary written to $(Join-Path $Scratch 'summary.txt')"
