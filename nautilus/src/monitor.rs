use std::time::Duration;

use libafl::monitors::{Monitor, ClientStats};
use libafl::bolts::{current_time, format_duration_hms};

pub struct CoverageMonitor<F: FnMut(String)> {
    print_fn: F,
    start_time: Duration,
    client_stats: Vec<ClientStats>,
}


impl<F: FnMut(String)> CoverageMonitor<F>  {
  pub fn new(println_fn: F) -> Self {
    Self {
      print_fn: println_fn,
      start_time: current_time(),
      client_stats: vec![],
    }
  }
}

impl<F: FnMut(String)> Monitor for CoverageMonitor<F> {
    fn client_stats_mut(&mut self) -> &mut Vec<libafl::monitors::ClientStats> {
      &mut self.client_stats
    }

    fn client_stats(&self) -> &[libafl::monitors::ClientStats] {
      &self.client_stats
    }

    fn start_time(&mut self) -> std::time::Duration {
      self.start_time
    }

    fn display(&mut self, event_msg: String, sender_id: u32) {
        let fmt = format!(
            "[{} #{}] run time: {}, clients: {}, corpus: {}, objectives: {}, executions: {}, exec/sec: {}, ",
            event_msg,
            sender_id,
            format_duration_hms(&(current_time() - self.start_time)),
            self.client_stats().len(),
            self.corpus_size(),
            self.objective_size(),
            self.total_execs(),
            self.execs_per_sec()
        );
        let result = fmt + &self.client_stats[0].user_monitor.iter().map(|x| {
          format!("{}: {}", x.0, x.1)
        }).collect::<Vec<String>>().join(", ");
        (self.print_fn)(result);
    }
}
