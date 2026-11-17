//! Linux (LIN-08): сторона и состав кнопок окна — настройка среды `button-layout` (`org.gnome.desktop.wm.preferences`,
//! XDG Settings portal; строка вида «close,minimize:maximize»). Разбор — `chrome::parse_buttons`. Portal или ключа
//! нет — кнопки справа, все три (как на Windows), INFO в журнал. Смена настройки — на лету.

use ashpd::desktop::settings::Settings;
use futures::StreamExt;
use gpui_kit::App;

/// Пространство и ключ настройки GNOME о кнопках окна.
const NAMESPACE: &str = "org.gnome.desktop.wm.preferences";
const KEY: &str = "button-layout";

/// Прочитать настройку и следить за ней в своём потоке; каждое значение — в [`crate::chrome::set_buttons`]
/// (поток интерфейса) с перерисовкой окон.
pub fn watch(cx: &mut App) {
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<String>();
    let spawned = std::thread::Builder::new().name("wm-buttons".into()).spawn(move || {
        futures::executor::block_on(async move {
            let settings = match Settings::new().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::info!("wm buttons: settings portal not available, buttons on the right: {e}");
                    return;
                }
            };
            match settings.read::<String>(NAMESPACE, KEY).await {
                Ok(layout) => {
                    tracing::debug!(layout, "wm buttons: button-layout");
                    if tx.unbounded_send(layout).is_err() {
                        return;
                    }
                }
                Err(e) => tracing::info!("wm buttons: no button-layout key, buttons on the right: {e}"),
            }
            let Ok(mut changes) = settings.receive_setting_changed_with_args::<String>(NAMESPACE, KEY).await else {
                tracing::info!("wm buttons: button-layout changes are not watched");
                return;
            };
            while let Some(change) = changes.next().await {
                match change {
                    Ok(layout) => {
                        tracing::info!(layout, "wm buttons: button-layout changed");
                        if tx.unbounded_send(layout).is_err() {
                            return;
                        }
                    }
                    Err(e) => tracing::warn!("wm buttons: button-layout change not read: {e}"),
                }
            }
        });
    });
    if let Err(e) = spawned {
        tracing::warn!("wm buttons: watcher thread not started: {e}");
        return;
    }
    cx.spawn(async move |cx| {
        while let Some(layout) = rx.next().await {
            cx.update(|cx| crate::chrome::set_buttons(&layout, cx));
        }
    })
    .detach();
}
