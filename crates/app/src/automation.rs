//! Scripted keystrokes and offscreen screenshots, for checking the UI without a human.
//! Build with `--features screenshot` and set `UBERGIT_SCRIPT`, e.g.
//! `wait 2000; shot /tmp/a.png; keys l j; type hello; key enter; shot /tmp/b.png; quit`.

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
