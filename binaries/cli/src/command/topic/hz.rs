use crossterm::event::{Event, KeyCode, KeyModifiers};
use dora_core::topics::{open_zenoh_session, zenoh_output_publish_topic};
use dora_message::{common::Timestamped, daemon_to_daemon::InterDaemonEvent};
use eyre::{Context, eyre};
use itertools::Itertools;
use ratatui::{DefaultTerminal, prelude::*, widgets::*};
use std::{
    borrow::Cow,
    collections::{BTreeMap, VecDeque},
    iter,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::watch;
use uuid::Uuid;

use crate::command::topic::selector::{TopicIdentifier, TopicSelector, TopicSubscriptionContext};
use crate::{command::Executable, common::CoordinatorOptions};

/// Measure topic publish intervals.
///
/// Subscribe to one or more outputs and display per-topic interval statistics
/// (average, min, max, stddev) over a sliding window. Average frequency (Hz)
/// is derived from the average interval.
///
/// If no `DATA` is provided, all outputs from the selected dataflow will be
/// echoed.
///
/// Examples:
///
/// Measure a single topic:
///   dora topic hz -d my-dataflow robot1/pose
///
/// Measure multiple topics with a short window:
///   dora topic hz -d my-dataflow robot1/pose robot2/vel --window 5
///
/// Measure all topics:
///   dora topic hz -d my-dataflow --window 10
///
/// Note: The dataflow descriptor must include the following snippet so that
/// runtime messages can be inspected:
///
/// ```yaml
/// _unstable_debug:
///   publish_all_messages_to_zenoh: true
/// ```
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment)]
pub struct Hz {
    #[clap(flatten)]
    selector: TopicSelector,

    /// Sliding window size in seconds
    #[clap(long, default_value_t = 10)]
    window: usize,

    #[clap(flatten)]
    coordinator: CoordinatorOptions,
}

impl Executable for Hz {
    fn execute(self) -> eyre::Result<()> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("tokio runtime failed")?;
        let terminal = ratatui::init();
        rt.block_on(async move {
            let mut session = self.coordinator.connect()?;
            let zenoh_session = open_zenoh_session(Some(self.coordinator.coordinator_addr))
                .await
                .context("failed to open zenoh session")?;
            let ctx = self
                .selector
                .resolve(session.as_mut(), zenoh_session)
                .context("failed to resolve topics")?;
            run_hz(terminal, self.window, ctx).await
        })
        .inspect(|_| {
            ratatui::restore();
        })?;

        Ok(())
    }
}

#[derive(Debug)]
struct HzStats {
    timestamps: Mutex<VecDeque<Instant>>,
    window_duration: Duration,
}

impl HzStats {
    fn new(window_secs: usize) -> Self {
        Self {
            timestamps: Mutex::new(VecDeque::new()),
            window_duration: Duration::from_secs(window_secs as u64),
        }
    }

    fn record(&self, now: Instant) {
        let mut timestamps = self.timestamps.lock().unwrap();
        timestamps.push_back(now);
        let cutoff = now - self.window_duration;
        while let Some(&first) = timestamps.front() {
            if first < cutoff {
                timestamps.pop_front();
            } else {
                break;
            }
        }
    }

    fn intervals_ms(&self) -> Vec<f64> {
        // Return inter-arrival times in milliseconds for the current window
        self.timestamps
            .lock()
            .unwrap()
            .iter()
            .tuple_windows()
            .filter_map(|(a, b)| {
                let dt = b.duration_since(*a).as_secs_f64() * 1000.0;
                if dt > 0.0 { Some(dt) } else { None }
            })
            .collect()
    }

    fn calculate(&self) -> Option<Stats> {
        let intervals = self.intervals_ms();
        if intervals.is_empty() {
            return None;
        }

        let sum: f64 = intervals.iter().sum();
        let avg_ms = sum / intervals.len() as f64;

        let min_ms = intervals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max_ms = intervals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

        let variance =
            intervals.iter().map(|x| (x - avg_ms).powi(2)).sum::<f64>() / intervals.len() as f64;
        let std_ms = variance.sqrt();

        let avg_hz = if avg_ms > 0.0 { 1000.0 / avg_ms } else { 0.0 };

        Some(Stats {
            avg_ms,
            avg_hz,
            min_ms,
            max_ms,
            std_ms,
        })
    }
}

#[derive(Debug)]
struct Stats {
    avg_ms: f64,
    avg_hz: f64,
    min_ms: f64,
    max_ms: f64,
    std_ms: f64,
}

async fn run_hz(
    mut terminal: DefaultTerminal,
    window: usize,
    ctx: TopicSubscriptionContext,
) -> eyre::Result<()> {
    // Build per-topic stats map and a separate aggregate (ALL) entry.
    let mut stats_map: BTreeMap<TopicIdentifier, Arc<HzStats>> = BTreeMap::new();
    for topic in &ctx.topics {
        stats_map.insert(topic.clone(), Arc::new(HzStats::new(window)));
    }
    let all_key = TopicIdentifier {
        node_id: "<ALL>".to_string().into(),
        data_id: "*".to_string().into(),
    };
    let all_stats = Arc::new(HzStats::new(window));
    stats_map.insert(all_key.clone(), all_stats.clone());

    // Ordered view for UI (ALL first, then others)
    let mut ordered_keys: Vec<TopicIdentifier> = stats_map.keys().cloned().collect();
    ordered_keys.sort();
    if let Some(pos) = ordered_keys.iter().position(|k| k == &all_key) {
        ordered_keys.remove(pos);
    }
    ordered_keys.insert(0, all_key.clone());

    let stats: Vec<(&TopicIdentifier, Arc<HzStats>)> = ordered_keys
        .iter()
        .map(|k| (k, stats_map.get(k).unwrap().clone()))
        .collect();

    let mut selected: usize = 0;
    // Ssub-window for instantaneous rate (Hz)
    let sub_window = Duration::from_millis(200);
    // Keep a flowing sparkline per topic for recent rates
    let mut rate_series: Vec<VecDeque<u64>> = vec![VecDeque::with_capacity(240); stats.len()];
    // Start time to decide whether full window elapsed
    let start = Instant::now();

    terminal.draw(|f| {
        ui(
            f,
            &stats,
            selected,
            &rate_series,
            start,
            Duration::from_secs(window as u64),
        )
    })?;

    // Spawn subscribers for each output with shutdown signal (excluding ALL)
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    // for topic_id in &ctx.topics {
    let mut data_join = ctx.spawn_for_each_topic(move |zenoh_session, dataflow_id, topic_id| {
        let per_topic = stats_map
            .get(&topic_id)
            .cloned()
            .unwrap_or_else(|| Arc::new(HzStats::new(window)));
        let all_stats = all_stats.clone();
        let rx = shutdown_rx.clone();
        let topic = topic_id.clone();
        async move {
            subscribe_output(
                zenoh_session,
                dataflow_id,
                &topic,
                per_topic,
                Some(all_stats),
                rx,
            )
            .await
        }
    });

    loop {
        // Update per-topic instantaneous rate and append to series
        let now = Instant::now();
        for (i, (_topic, s)) in stats.iter().enumerate() {
            // count messages within sub_window
            let cutoff = now - sub_window;
            let mut count = 0usize;
            let ts = s.timestamps.lock().unwrap();
            for &t in ts.iter().rev() {
                if t < cutoff {
                    break;
                }
                count += 1;
            }
            let hz = (count as f64) / sub_window.as_secs_f64();
            let v = hz.max(0.0).round() as u64;
            let buf = &mut rate_series[i];
            if buf.len() >= 240 {
                buf.pop_front();
            }
            buf.push_back(v);
        }

        terminal.draw(|f| {
            ui(
                f,
                &stats,
                selected,
                &rate_series,
                start,
                Duration::from_secs(window as u64),
            )
        })?;

        if crossterm::event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = crossterm::event::read()? {
                if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                    || (key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c'))
                {
                    break;
                }

                match key.code {
                    KeyCode::Up => {
                        if selected == 0 {
                            selected = stats.len().saturating_sub(1);
                        } else {
                            selected -= 1;
                        }
                    }
                    KeyCode::Down => {
                        if stats.is_empty() {
                            selected = 0;
                        } else {
                            selected = (selected + 1) % stats.len();
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Signal shutdown to subscribers
    let _ = shutdown_tx.send(true);
    while let Some(res) = data_join.join_next().await {
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                eprintln!("Error while inspecting output: {e}");
            }
            Err(e) => {
                eprintln!("Join error: {e}");
            }
        }
    }

    Ok(())
}

async fn subscribe_output(
    zenoh_session: zenoh::Session,
    dataflow_id: Uuid,
    topic: &TopicIdentifier,
    hz_stats: Arc<HzStats>,
    aggregate: Option<Arc<HzStats>>,
    mut shutdown: watch::Receiver<bool>,
) -> eyre::Result<()> {
    let subscribe_topic = zenoh_output_publish_topic(dataflow_id, &topic.node_id, &topic.data_id);
    let subscriber = zenoh_session
        .declare_subscriber(subscribe_topic)
        .await
        .map_err(|e| eyre!(e))
        .wrap_err_with(|| format!("failed to subscribe to {topic}"))?;

    loop {
        tokio::select! {
            res = subscriber.recv_async() => {
                let sample = match res { Ok(s) => s, Err(_) => break };
                let event = match Timestamped::deserialize_inter_daemon_event(&sample.payload().to_bytes()) { Ok(e) => e, Err(_) => continue };
                match event.inner {
                    InterDaemonEvent::Output { .. } => {
                        let now = Instant::now();
                        hz_stats.record(now);
                        if let Some(all) = &aggregate { all.record(now); }
                    }
                    InterDaemonEvent::OutputClosed { .. } => { break; }
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
        }
    }

    Ok(())
}

fn ui(
    f: &mut Frame<'_>,
    stats: &[(&TopicIdentifier, Arc<HzStats>)],
    selected: usize,
    rate_series: &[VecDeque<u64>],
    start: Instant,
    window_dur: Duration,
) {
    // Table header: interval stats in ms + derived avg Hz
    let header = Row::new([
        "Output", "Avg (ms)", "Avg (Hz)", "Min (ms)", "Max (ms)", "Std (ms)",
    ])
    .style(Style::default().fg(Color::White).bg(Color::Blue).bold())
    .height(1);

    let rows = stats
        .iter()
        .enumerate()
        .map(|(i, (output_name, hz_stats))| {
            if let Some(s) = hz_stats.calculate() {
                Row::new([
                    output_name.to_string(),
                    format!("{:.2}", s.avg_ms),
                    format!("{:.2}", s.avg_hz),
                    format!("{:.2}", s.min_ms),
                    format!("{:.2}", s.max_ms),
                    format!("{:.2}", s.std_ms),
                ])
                .style(if i == selected {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                })
            } else {
                Row::new(
                    iter::once(Cow::Owned(output_name.to_string()))
                        .chain(iter::repeat_n(Cow::Borrowed("-"), 5)),
                )
                .style(if i == selected {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                })
            }
            .height(1)
        });

    let table = Table::new(
        rows,
        [
            Constraint::Fill(1),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
        ],
    )
    .header(header);

    // Reserve space for a one-line footer with shortcut hints.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(15), Constraint::Length(1)])
        .split(f.area());

    let table_and_chart = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[0]);

    f.render_widget(table, table_and_chart[0]);

    // Charts area split horizontally
    let chart_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(table_and_chart[1]);

    // Draw charts for the selected topic if available
    if let Some((name, selected_stats)) = stats.get(selected) {
        // Prepare data
        let intervals = selected_stats.intervals_ms();
        let now = Instant::now();

        // Left: Rolling rate sparkline (Hz within sub-window)
        let mut series: Vec<u64> = rate_series
            .get(selected)
            .map(|d| d.iter().copied().collect())
            .unwrap_or_default();
        if series.is_empty() {
            // Show hint while not enough samples
            let info = Paragraph::new("Waiting for data...")
                .style(Style::default().fg(Color::Gray).italic())
                .block(
                    Block::default()
                        .title("Recent Rate (Hz)")
                        .borders(Borders::ALL),
                );
            f.render_widget(info, chart_chunks[0]);
        } else {
            // Fit series to available width
            let w = chart_chunks[0].width.saturating_sub(2) as usize; // borders
            if series.len() > w {
                series = series[series.len() - w..].to_vec();
            }
            let spark = Sparkline::default()
                .data(&series)
                .style(Style::default().fg(Color::Cyan))
                .block(
                    Block::default()
                        .title(format!("Recent Rate (Hz) — {}", name))
                        .borders(Borders::ALL),
                );
            f.render_widget(spark, chart_chunks[0]);
        }

        // Right: Histogram (s) using BarChart
        if intervals.is_empty() {
            let info = Paragraph::new("No samples for histogram")
                .style(Style::default().fg(Color::Gray).italic())
                .block(
                    Block::default()
                        .title("Histogram (ms)")
                        .borders(Borders::ALL),
                );
            f.render_widget(info, chart_chunks[1]);
        } else {
            let min = intervals.iter().cloned().fold(f64::INFINITY, f64::min);
            let max = intervals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let bins = 10usize
                .max((chart_chunks[1].width as usize).saturating_sub(8) / 4)
                .min(40);
            let span = (max - min).max(1e-9);
            let step = span / bins as f64;
            let mut counts = vec![0u64; bins];
            for &v in &intervals {
                let mut idx = ((v - min) / step).floor() as usize;
                if idx >= bins {
                    idx = bins - 1;
                }
                counts[idx] += 1;
            }

            let bars: Vec<Bar<'_>> = counts
                .iter()
                .enumerate()
                .map(|(i, &c)| {
                    let lo = min + i as f64 * step;
                    let hi = lo + step;
                    Bar::default()
                        .value(c)
                        .label(format!("{:.3}-{:.3}", lo, hi).into())
                        .style(Style::default().fg(Color::Green))
                })
                .collect();

            let group = BarGroup::default().bars(&bars);
            let barchart = BarChart::default()
                .block(
                    Block::default()
                        .title(format!("Histogram (ms) — min={:.2}, max={:.2}", min, max))
                        .borders(Borders::ALL),
                )
                .data(group)
                .bar_width(3)
                .bar_gap(1);
            f.render_widget(barchart, chart_chunks[1]);
        }

        // If full window time not elapsed since start, render hint
        if now.duration_since(start) + Duration::from_millis(1) < window_dur {
            let warn = Paragraph::new(format!(
                "Filling window: {:.0}/{:.0} ms",
                now.duration_since(start).as_secs_f64() * 1000.0,
                window_dur.as_secs_f64() * 1000.0
            ))
            .style(Style::default().fg(Color::Yellow))
            .alignment(Alignment::Center);
            f.render_widget(warn, chunks[1]);
        }
    } else {
        // Nothing selected or empty stats
        let info = Paragraph::new("No topics selected")
            .style(Style::default().fg(Color::Gray))
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL));
        f.render_widget(info, chunks[1]);
    }

    // Footer with key hints
    let footer = Paragraph::new("Up/Down: Select  |  Exit: q / Ctrl-C / Esc")
        .style(Style::default().fg(Color::Yellow))
        .alignment(Alignment::Center);
    f.render_widget(footer, chunks[1]);
}
