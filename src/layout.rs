//! Pure planning for multi-screen sessions: which local monitor shows which
//! server output, and where a window for it goes.

use crate::display::LocalMonitor;
use st_protocol::{control::OutputInfo, OUTPUT_VIRTUAL};
use std::time::Duration;

/// How long a plan may wait for the main window's position.
pub const POSITION_WAIT: Duration = Duration::from_millis(1500);

/// Whether the layout may be planned now. Where the window system reports
/// window positions, the main window's monitor is waited for (bounded) rather
/// than guessed, since a plan built around the wrong monitor opens a window
/// on the main window's own screen. Where it never reports one (Wayland),
/// the fallback is used at once.
pub fn position_settled(known: bool, positions_reported: bool, waited: Duration) -> bool {
    known || !positions_reported || waited >= POSITION_WAIT
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuxTarget {
    Output(u32),
    /// A virtual output the server creates for the session, sized like the
    /// monitor that shows it.
    Virtual,
}

impl AuxTarget {
    /// `ClientDisplayInfo::output_id` that asks the server for this target.
    pub fn requested_output(self) -> u32 {
        match self {
            Self::Output(id) => id,
            Self::Virtual => OUTPUT_VIRTUAL,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuxPlan {
    /// Index into the local monitor list.
    pub monitor: usize,
    pub target: AuxTarget,
}

/// Gives every local monitor but the one the main window sits on a server
/// output. The main window shows `root_output`; the rest pair up in
/// left-to-right, top-to-bottom order. A local monitor left without an output
/// gets a virtual one when the server can create them and stays unused
/// otherwise. Surplus server outputs stay available in the menu.
pub fn plan_auto_layout(
    monitors: &[LocalMonitor],
    outputs: &[OutputInfo],
    root_monitor: usize,
    root_output: u32,
    virtual_supported: bool,
) -> Vec<AuxPlan> {
    let mut locals: Vec<usize> = (0..monitors.len()).filter(|i| *i != root_monitor).collect();
    locals.sort_by_key(|&i| (monitors[i].x, monitors[i].y, monitors[i].name.clone()));

    let mut servers: Vec<&OutputInfo> = outputs
        .iter()
        .filter(|o| !o.is_virtual && o.id != root_output)
        .collect();
    servers.sort_by_key(|o| (o.x, o.y, o.id));

    locals
        .into_iter()
        .enumerate()
        .filter_map(|(rank, monitor)| {
            let target = match servers.get(rank) {
                Some(output) => AuxTarget::Output(output.id),
                None if virtual_supported => AuxTarget::Virtual,
                None => return None,
            };
            Some(AuxPlan { monitor, target })
        })
        .collect()
}

/// Whether the server can offer another virtual display to a monitor that has
/// no output of its own.
pub fn can_add_virtual_display(
    monitors: &[LocalMonitor],
    outputs: &[OutputInfo],
    virtual_supported: bool,
) -> bool {
    virtual_supported && monitors.len() > outputs.len()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowPlacement {
    /// Top-left in desktop coordinates; `None` leaves it to the compositor.
    pub position: Option<(f32, f32)>,
    /// Inner size in points before any fullscreen.
    pub size: (f32, f32),
    pub fullscreen: bool,
}

/// Where the window for a monitor goes. With placement control the window
/// opens on the monitor and goes fullscreen there; without it, it opens as an
/// ordinary window in the output's aspect that the user moves.
pub fn window_placement(
    monitor: &LocalMonitor,
    picture: (u32, u32),
    can_place: bool,
) -> WindowPlacement {
    let (monitor_w, monitor_h) = monitor.logical_size();
    if can_place {
        return WindowPlacement {
            position: Some((monitor.x as f32 + 40.0, monitor.y as f32 + 40.0)),
            size: ((monitor_w * 0.6).max(320.0), (monitor_h * 0.6).max(200.0)),
            fullscreen: true,
        };
    }
    let aspect = if picture.0 > 0 && picture.1 > 0 {
        picture.0 as f32 / picture.1 as f32
    } else {
        monitor_w / monitor_h.max(1.0)
    };
    let max_w = (monitor_w * 0.6).max(320.0);
    let max_h = (monitor_h * 0.6).max(200.0);
    let width = max_w.min(max_h * aspect);
    WindowPlacement {
        position: None,
        size: (width, width / aspect),
        fullscreen: false,
    }
}

/// An ordinary window for an output opened by hand: the output's aspect, at
/// most 960x600 points.
pub fn floating_placement(picture: (u32, u32)) -> WindowPlacement {
    let aspect = if picture.0 > 0 && picture.1 > 0 {
        picture.0 as f32 / picture.1 as f32
    } else {
        16.0 / 9.0
    };
    let width = 960.0_f32.min(600.0 * aspect);
    WindowPlacement {
        position: None,
        size: (width, width / aspect),
        fullscreen: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: &str, x: i32, y: i32, w: u32, h: u32, primary: bool) -> LocalMonitor {
        LocalMonitor {
            name: name.into(),
            x,
            y,
            width: w,
            height: h,
            refresh_millihz: Some(60_000),
            scale: 1.0,
            primary,
        }
    }

    fn output(id: u32, x: i32, w: u32, h: u32, rotation: u8) -> OutputInfo {
        OutputInfo {
            id,
            name: format!("OUT-{id}"),
            width: w,
            height: h,
            x,
            y: 0,
            is_primary: id == 1,
            is_virtual: false,
            refresh_millihz: 60_000,
            rotation,
        }
    }

    fn landscape(count: i32) -> Vec<LocalMonitor> {
        (0..count)
            .map(|i| monitor(&format!("M{i}"), i * 1920, 0, 1920, 1080, i == 0))
            .collect()
    }

    fn server(count: u32) -> Vec<OutputInfo> {
        (1..=count)
            .map(|id| output(id, (id as i32 - 1) * 1920, 1920, 1080, 0))
            .collect()
    }

    #[test]
    fn the_plan_waits_for_a_reported_window_position_but_not_forever() {
        let none = Duration::ZERO;
        assert!(position_settled(true, true, none), "known position");
        assert!(
            position_settled(false, false, none),
            "never reported: no wait"
        );
        assert!(
            !position_settled(false, true, none),
            "reported but not yet known"
        );
        assert!(!position_settled(false, true, POSITION_WAIT / 2));
        assert!(position_settled(false, true, POSITION_WAIT), "gives up");
    }

    #[test]
    fn one_local_monitor_opens_nothing() {
        assert!(plan_auto_layout(&landscape(1), &server(2), 0, 1, true).is_empty());
    }

    #[test]
    fn two_local_two_server_pairs_the_second() {
        let plan = plan_auto_layout(&landscape(2), &server(2), 0, 1, false);
        assert_eq!(
            plan,
            [AuxPlan {
                monitor: 1,
                target: AuxTarget::Output(2)
            }]
        );
    }

    #[test]
    fn three_local_two_server_adds_virtual_only_when_supported() {
        let with_virtual = plan_auto_layout(&landscape(3), &server(2), 0, 1, true);
        assert_eq!(
            with_virtual,
            [
                AuxPlan {
                    monitor: 1,
                    target: AuxTarget::Output(2)
                },
                AuxPlan {
                    monitor: 2,
                    target: AuxTarget::Virtual
                },
            ]
        );
        let without = plan_auto_layout(&landscape(3), &server(2), 0, 1, false);
        assert_eq!(
            without,
            [AuxPlan {
                monitor: 1,
                target: AuxTarget::Output(2)
            }]
        );
    }

    #[test]
    fn two_local_one_server_uses_a_virtual_display() {
        let plan = plan_auto_layout(&landscape(2), &server(1), 0, 1, true);
        assert_eq!(
            plan,
            [AuxPlan {
                monitor: 1,
                target: AuxTarget::Virtual
            }]
        );
        assert!(plan_auto_layout(&landscape(2), &server(1), 0, 1, false).is_empty());
    }

    #[test]
    fn root_monitor_and_root_output_are_excluded_wherever_they_sit() {
        // The main window is on the middle monitor and shows server output 2.
        let plan = plan_auto_layout(&landscape(3), &server(3), 1, 2, false);
        assert_eq!(
            plan,
            [
                AuxPlan {
                    monitor: 0,
                    target: AuxTarget::Output(1)
                },
                AuxPlan {
                    monitor: 2,
                    target: AuxTarget::Output(3)
                },
            ]
        );
    }

    #[test]
    fn outputs_pair_by_desktop_position_not_list_order() {
        let outputs = vec![
            output(7, 3840, 1920, 1080, 0),
            output(1, 0, 1920, 1080, 0),
            output(5, 1920, 1920, 1080, 0),
        ];
        let plan = plan_auto_layout(&landscape(3), &outputs, 0, 1, false);
        assert_eq!(plan[0].target, AuxTarget::Output(5));
        assert_eq!(plan[1].target, AuxTarget::Output(7));
    }

    #[test]
    fn rotated_outputs_pair_by_position_and_keep_their_upright_size() {
        // A portrait second output (rotation 1, 1440x2560 upright) next to a
        // landscape local monitor: pairing ignores orientation, and the window
        // for it is sized from the upright picture.
        let outputs = vec![output(1, 0, 2560, 1440, 0), output(2, 2560, 1440, 2560, 1)];
        let monitors = vec![
            monitor("L0", 0, 0, 2560, 1440, true),
            monitor("L1", 2560, 0, 1920, 1080, false),
        ];
        let plan = plan_auto_layout(&monitors, &outputs, 0, 1, false);
        assert_eq!(
            plan,
            [AuxPlan {
                monitor: 1,
                target: AuxTarget::Output(2)
            }]
        );
        let placement = window_placement(&monitors[1], (1440, 2560), false);
        assert!(placement.size.1 > placement.size.0, "portrait window");
        assert!((placement.size.0 / placement.size.1 - 1440.0 / 2560.0).abs() < 0.01);
    }

    #[test]
    fn virtual_outputs_of_other_sessions_are_not_auto_assigned() {
        let mut outputs = server(2);
        outputs.push(OutputInfo {
            is_virtual: true,
            ..output(9, 3840, 1280, 720, 0)
        });
        let plan = plan_auto_layout(&landscape(3), &outputs, 0, 1, false);
        assert_eq!(plan.len(), 1);
    }

    #[test]
    fn floating_windows_follow_the_output_aspect() {
        let landscape = floating_placement((2560, 1440));
        assert_eq!(landscape.size.0, 960.0);
        let portrait = floating_placement((1440, 2560));
        assert!(portrait.size.1 <= 600.0 + 0.01 && portrait.size.1 > portrait.size.0);
        assert!(!portrait.fullscreen && portrait.position.is_none());
        assert_eq!(floating_placement((0, 0)).size.0, 960.0);
    }

    #[test]
    fn virtual_target_requests_the_virtual_sentinel() {
        assert_eq!(AuxTarget::Virtual.requested_output(), OUTPUT_VIRTUAL);
        assert_eq!(AuxTarget::Output(42).requested_output(), 42);
    }

    #[test]
    fn virtual_display_is_offered_while_monitors_outnumber_outputs() {
        assert!(can_add_virtual_display(&landscape(3), &server(2), true));
        assert!(!can_add_virtual_display(&landscape(2), &server(2), true));
        assert!(!can_add_virtual_display(&landscape(3), &server(2), false));
    }

    #[test]
    fn placement_goes_fullscreen_only_where_the_window_can_be_placed() {
        let m = monitor("M1", 1920, 0, 2560, 1440, false);
        let placed = window_placement(&m, (1920, 1080), true);
        assert!(placed.fullscreen);
        let (x, y) = placed.position.expect("position");
        assert!(x > 1920.0 && y >= 0.0, "inside the monitor");
        let unplaced = window_placement(&m, (1920, 1080), false);
        assert!(!unplaced.fullscreen);
        assert_eq!(unplaced.position, None);
        assert!(unplaced.size.0 <= 2560.0 * 0.6 + 0.5);
    }
}
