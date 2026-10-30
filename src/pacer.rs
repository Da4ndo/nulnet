use crate::telemetry::{Event, EventKind, Phase, Probe};
use std::time::{Duration, Instant};

const CPU_JUMP: f64 = 20.0;
const MEM_JUMP: f64 = 10.0;
const GPU_JUMP: f64 = 20.0;

const NET_SPIKE_MULT: f64 = 4.0;
const NET_SPIKE_MIN: f64 = 1024.0 * 1024.0;
const CONN_SURGE_MIN: f64 = 20.0;
const PPS_FLOOD_MULT: f64 = 5.0;
const PPS_FLOOD_MIN: f64 = 20_000.0;
const SYN_FLOOD: (u32, u32) = (100, 50);
const CALM_MULT: f64 = 2.0;

const CONFIRM_PROBES: u8 = 2;
const EMA_WINDOW_SECS: f64 = 50.0;

#[derive(Debug, Default)]
struct Ema(Option<f64>);

impl Ema {
	fn update(&mut self, x: f64, alpha: f64) {
		self.0 = Some(self.0.map_or(x, |v| v + alpha * (x - v)));
	}

	fn spikes(&self, x: f64, mult: f64, min: f64) -> bool {
		x >= min && self.0.is_some_and(|v| x >= v * mult)
	}

	fn calm(&self, x: f64, mult: f64, min: f64) -> bool {
		x < (self.0.unwrap_or(0.0) * mult).max(min)
	}
}

#[derive(Debug, Default)]
struct Condition {
	active: bool,
	streak: u8,
}

impl Condition {
	fn spike(&mut self, ema: &Ema, x: f64, by: f64) -> Option<Phase> {
		let base = ema.0;
		self.update(
			base.is_some_and(|v| x - v >= by),
			base.is_none_or(|v| x - v < by / 2.0),
			CONFIRM_PROBES,
		)
	}

	fn update(&mut self, start: bool, end: bool, confirm: u8) -> Option<Phase> {
		if self.active {
			self.active = !end;
			return end.then_some(Phase::End);
		}
		self.streak = if start { self.streak + 1 } else { 0 };
		if self.streak < confirm {
			return None;
		}
		self.active = true;
		self.streak = 0;
		Some(Phase::Start)
	}
}

pub struct Decision {
	pub persist: bool,
	pub events: Vec<Event>,
}

pub struct Pacer {
	base: Duration,
	tick: Duration,
	alpha: f64,
	last_persist: Option<Instant>,
	cpu_ema: Ema,
	mem_ema: Ema,
	gpu_ema: Ema,
	net_ema: Ema,
	conn_ema: Ema,
	pps_ema: Ema,
	cpu: Condition,
	mem: Condition,
	gpu: Condition,
	net: Condition,
	conn: Condition,
	ddos: Condition,
}

impl Pacer {
	pub fn new(base: Duration, tick: Duration) -> Self {
		Self {
			base,
			tick,
			alpha: 1.0 - (-tick.as_secs_f64() / EMA_WINDOW_SECS).exp(),
			last_persist: None,
			cpu_ema: Ema::default(),
			mem_ema: Ema::default(),
			gpu_ema: Ema::default(),
			net_ema: Ema::default(),
			conn_ema: Ema::default(),
			pps_ema: Ema::default(),
			cpu: Condition::default(),
			mem: Condition::default(),
			gpu: Condition::default(),
			net: Condition::default(),
			conn: Condition::default(),
			ddos: Condition::default(),
		}
	}

	pub fn observe(&mut self, p: &Probe, now: Instant) -> Decision {
		let events = self.detect(p);
		self.update_baselines(p);

		let persist = self
			.last_persist
			.is_none_or(|last| now.duration_since(last) >= self.base.saturating_sub(self.tick / 2));
		if persist {
			self.last_persist = Some(now);
		}
		Decision { persist, events }
	}

	fn detect(&mut self, p: &Probe) -> Vec<Event> {
		let net = p.network.rx_bytes_per_sec + p.network.tx_bytes_per_sec;
		let conns = p.network.new_connections_per_sec;
		let pps = p.network.rx_packets_per_sec;

		let mut events = vec![];
		let mut emit = |kind, phase: Option<Phase>, value| {
			if let Some(phase) = phase {
				events.push(Event { kind, phase, value, timestamp: p.timestamp });
			}
		};

		// Spikes are jumps above the moving average, not fixed levels: a steady high
		// load becomes the average and ends the spike.
		emit(EventKind::CpuSpike, self.cpu.spike(&self.cpu_ema, p.cpu, CPU_JUMP), p.cpu);
		emit(EventKind::MemSpike, self.mem.spike(&self.mem_ema, p.memory, MEM_JUMP), p.memory);
		if let Some(g) = p.gpu {
			emit(EventKind::GpuSpike, self.gpu.spike(&self.gpu_ema, g, GPU_JUMP), g);
		}

		let net_phase = self.net.update(
			self.net_ema.spikes(net, NET_SPIKE_MULT, NET_SPIKE_MIN),
			self.net_ema.calm(net, CALM_MULT, NET_SPIKE_MIN),
			CONFIRM_PROBES,
		);
		emit(EventKind::NetSpike, net_phase, net);
		let conn_phase = self.conn.update(
			self.conn_ema.spikes(conns, NET_SPIKE_MULT, CONN_SURGE_MIN),
			self.conn_ema.calm(conns, CALM_MULT, CONN_SURGE_MIN),
			CONFIRM_PROBES,
		);
		emit(EventKind::ConnSurge, conn_phase, conns);

		let flood = p.network.syn_recv >= SYN_FLOOD.0
			|| self.pps_ema.spikes(pps, PPS_FLOOD_MULT, PPS_FLOOD_MIN)
			|| (self.conn.active && self.net.active);
		let flood_over = p.network.syn_recv < SYN_FLOOD.1
			&& self.pps_ema.calm(pps, CALM_MULT, PPS_FLOOD_MIN)
			&& !(self.conn.active && self.net.active);
		let ddos = self.ddos.update(flood, flood_over, CONFIRM_PROBES);
		emit(EventKind::PossibleDdos, ddos, pps);
		events
	}

	/// Baselines freeze during an active condition so a long attack never becomes normal.
	fn update_baselines(&mut self, p: &Probe) {
		let (n, a) = (&p.network, self.alpha);
		self.cpu_ema.update(p.cpu, a);
		self.mem_ema.update(p.memory, a);
		if let Some(g) = p.gpu {
			self.gpu_ema.update(g, a);
		}
		if !self.net.active && !self.ddos.active {
			self.net_ema.update(n.rx_bytes_per_sec + n.tx_bytes_per_sec, a);
		}
		if !self.conn.active && !self.ddos.active {
			self.conn_ema.update(n.new_connections_per_sec, a);
		}
		if !self.ddos.active {
			self.pps_ema.update(n.rx_packets_per_sec, a);
		}
	}
}
