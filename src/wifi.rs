use defmt::{error, info};
use embassy_futures::select::Either;
use embassy_time::{Duration, Timer};
use esp_radio::wifi::WifiController;

#[embassy_executor::task]
pub async fn connection(mut controller: WifiController<'static>) {
    info!("start connection task");

    loop {
        match controller.connect_async().await {
            Ok(_) => {
                // wait until we're no longer connected
                loop {
                    let info = embassy_futures::select::select(
                        controller.wait_for_disconnect_async(),
                        controller.wait_for_access_point_connected_event_async(),
                    )
                    .await;

                    match info {
                        Either::First(station_disconnected) => {
                            if let Ok(station_disconnected) = station_disconnected {
                                info!("Station disconnected: {:?}", station_disconnected);
                            }
                            break;
                        }
                        Either::Second(event) => {
                            if let Ok(event) = event {
                                match event {
                                    esp_radio::wifi::ap::EventInfo::Connected(info) => {
                                        info!("Station connected: {:?}", info);
                                    }
                                    esp_radio::wifi::ap::EventInfo::Disconnected(info) => {
                                        info!("Station disconnected: {:?}", info);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                error!("Failed to connect to wifi: {:?}", e);
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}
