//! Scripted keystrokes and offscreen screenshots, for checking the UI without a human.
//! Build with `--features screenshot` and set `UBERGIT_SCRIPT`, e.g.
//! `wait 2000; shot /tmp/a.png; keys l j; type hello; key enter; shot /tmp/b.png; quit`.
//! Mouse steps take window points: `drag x1 y1 x2 y2`, `wheel x y pixels`.

use std::time::Duration;

use gpui_kit::*;

pub fn run(window: AnyWindowHandle, script: String, cx: &mut App) {
    cx.spawn(async move |cx| {
        let executor = cx.background_executor().clone();
        let pause = |ms: u64| executor.timer(Duration::from_millis(ms));
        for step in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let (command, arg) = step.split_once(' ').unwrap_or((step, ""));
            match command {
                "wait" => pause(arg.trim().parse().unwrap_or(500)).await,
                "key" | "keys" => {
                    for key in arg.split_whitespace() {
                        press(window, key, cx);
                        pause(80).await;
                    }
                }
                "type" => {
                    for ch in arg.chars() {
                        let key = match ch {
                            ' ' => "space".to_string(),
                            c if c.is_ascii_uppercase() => format!("shift-{}", c.to_ascii_lowercase()),
                            c => c.to_string(),
                        };
                        press(window, &key, cx);
                        pause(20).await;
                    }
                }
                "shot" => {
                    pause(100).await;
                    // macOS stops drawing occluded windows, so draw the frame ourselves
                    // rather than capturing whatever was last presented.
                    let image = window.update(cx, |_, window, cx| {
                        window.draw(cx).clear(cx);
                        window.render_to_image()
                    });
                    match image {
                        Ok(Ok(image)) => {
                            if let Err(err) = image.save(arg.trim()) {
                                eprintln!("screenshot {arg}: {err}");
                            } else {
                                eprintln!("screenshot saved: {arg}");
                            }
                        }
                        Ok(Err(err)) | Err(err) => eprintln!("screenshot failed: {err:#}"),
                    }
                }
                // drag x1 y1 x2 y2, in window points (half the screenshot's pixels).
                "drag" => {
                    let n: Vec<f32> = arg.split_whitespace().filter_map(|v| v.parse().ok()).collect();
                    let [x1, y1, x2, y2] = n[..] else {
                        eprintln!("automation: drag needs x1 y1 x2 y2");
                        continue;
                    };
                    let at = |x: f32, y: f32| point(px(x), px(y));
                    let mut events = vec![
                        PlatformInput::MouseMove(MouseMoveEvent { position: at(x1, y1), ..Default::default() }),
                        PlatformInput::MouseDown(MouseDownEvent {
                            button: MouseButton::Left,
                            position: at(x1, y1),
                            click_count: 1,
                            ..Default::default()
                        }),
                    ];
                    for step in 1..=10 {
                        let t = step as f32 / 10.;
                        events.push(PlatformInput::MouseMove(MouseMoveEvent {
                            position: at(x1 + (x2 - x1) * t, y1 + (y2 - y1) * t),
                            pressed_button: Some(MouseButton::Left),
                            ..Default::default()
                        }));
                    }
                    events.push(PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position: at(x2, y2),
                        click_count: 1,
                        ..Default::default()
                    }));
                    for event in events {
                        window
                            .update(cx, |_, window, cx| {
                                window.dispatch_event(event, cx);
                                // Let hitboxes and drag state settle between events.
                                window.draw(cx).clear(cx);
                            })
                            .ok();
                        pause(16).await;
                    }
                }
                // wheel x y pixels: scroll at a window point; positive pixels scroll down.
                "wheel" => {
                    let n: Vec<f32> = arg.split_whitespace().filter_map(|v| v.parse().ok()).collect();
                    let [x, y, pixels] = n[..] else {
                        eprintln!("automation: wheel needs x y pixels");
                        continue;
                    };
                    let event = PlatformInput::ScrollWheel(ScrollWheelEvent {
                        position: point(px(x), px(y)),
                        delta: ScrollDelta::Pixels(point(px(0.), px(-pixels))),
                        ..Default::default()
                    });
                    window
                        .update(cx, |_, window, cx| {
                            window.dispatch_event(PlatformInput::MouseMove(MouseMoveEvent {
                                position: point(px(x), px(y)),
                                ..Default::default()
                            }), cx);
                            window.draw(cx).clear(cx);
                            window.dispatch_event(event, cx);
                        })
                        .ok();
                }
                "quit" => {
                    cx.update(|cx| cx.quit());
                }
                other => eprintln!("automation: unknown step {other:?}"),
            }
        }
    })
    .detach();
}

fn press(window: AnyWindowHandle, key: &str, cx: &mut AsyncApp) {
    match Keystroke::parse(key) {
        Ok(keystroke) => {
            window
                .update(cx, |_, window, cx| window.dispatch_keystroke(keystroke, cx))
                .ok();
        }
        Err(err) => eprintln!("automation: bad key {key:?}: {err}"),
    }
}
