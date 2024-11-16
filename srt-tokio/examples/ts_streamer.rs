#[cfg(feature = "ac-ffmpeg")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use std::{
        env::args,
        fs::File,
        io::{self, Write},
        time::{Duration, Instant},
    };

    use ac_ffmpeg::{
        format::{
            demuxer::Demuxer,
            io::IO,
            muxer::{Muxer, OutputFormat},
        },
        time::Timestamp,
    };
    use bytes::Bytes;
    use futures::SinkExt;
    use srt_tokio::SrtSocket;
    use tokio::{
        runtime::Handle,
        sync::mpsc::{channel, Sender},
        time::sleep_until,
    };
    use tokio_stream::StreamExt;

    struct WriteBridge(Sender<(Instant, Bytes)>, std::fs::File);
    impl Write for WriteBridge {
        fn write(&mut self, w: &[u8]) -> Result<usize, std::io::Error> {
            self.1.write_all(w).unwrap();
            for chunk in w.chunks(1316) {
                // NOTE: Instant::now() is not ideal
                // This should be directly derived from the PTS of the packet, which is not availabe here,
                // it would require some refactoring
                let first = chunk[0];
                println!("aaa send srt data len {cnt}, first [0x{first:02x}]", cnt=chunk.len());
                if self
                    .0
                    .try_send((Instant::now(), Bytes::copy_from_slice(chunk)))
                    .is_err()
                {
                    println!("Sender was throttled and buffer exausted, dropping packet");
                }
            }
            Ok(w.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Handle::current().block_on(async { Ok(()) })
        }
    }

    pretty_env_logger::init();

    let args = args().collect::<Vec<_>>();
    let input = File::open(&args[1])?; // validate file before connecting
    let io = IO::from_seekable_read_stream(input);

    let mut demuxer = Demuxer::builder()
        .build(io)?
        .find_stream_info(None)
        .map_err(|(_, err)| err)?;

    for (index, stream) in demuxer.streams().iter().enumerate() {
        let params = stream.codec_parameters();

        println!("Stream #{index}:");
        println!("  duration: {}", stream.duration().as_f64().unwrap_or(0f64));

        if let Some(params) = params.as_audio_codec_parameters() {
            println!("  type: audio");
            println!("  codec: {}", params.decoder_name().unwrap_or("N/A"));
            println!("  sample format: {}", params.sample_format().name());
            println!("  sample rate: {}", params.sample_rate());
            println!("  channels: {}", params.channel_layout().channels());
        } else if let Some(params) = params.as_video_codec_parameters() {
            println!("  type: video");
            println!("  codec: {}", params.decoder_name().unwrap_or("N/A"));
            println!("  width: {}", params.width());
            println!("  height: {}", params.height());
            println!("  pixel format: {}", params.pixel_format().name());
        } else {
            println!("  type: unknown");
        }
    }

    println!("Waiting for a connection to start streaming...");

    let mut socket = SrtSocket::builder()
        .latency(Duration::from_millis(1000))
        .listen_on(":1234")
        .await?;

    println!("Connection established");

    let mut last_pts_inst: Option<(Timestamp, Instant)> = None;

    let (chan_send, chan_recv) = channel(1024);

    let demuxer_task = tokio::spawn(async move {
        let streams = demuxer
            .streams()
            .iter()
            .map(|stream| stream.codec_parameters())
            .collect::<Vec<_>>();

        let file = std::fs::File::create("/tmp/output.ts").unwrap();
        let io = IO::from_write_stream(WriteBridge(chan_send, file));

        let mut muxer_builder = Muxer::builder();
        for codec_parameters in streams {
            muxer_builder.add_stream(&codec_parameters).unwrap();
        }

        let mut muxer = muxer_builder
            .build(io, OutputFormat::find_by_name("mpegts").unwrap())
            .unwrap();

        while let Some(packet) = demuxer.take().unwrap() {
            let pts = packet.pts();
            let _inst = match last_pts_inst {
                Some((last_pts, last_inst)) => {
                    if pts < last_pts {
                        last_inst
                    } else {
                        let d_t = pts - last_pts;
                        let deadline = last_inst + d_t;
                        sleep_until(deadline.into()).await;
                        last_pts_inst = Some((pts, deadline));
                        deadline
                    }
                }
                None => {
                    let now = Instant::now();
                    last_pts_inst = Some((pts, now));
                    now
                }
            };
            println!(
                "Sending packet {:?} len={}",
                packet.pts(),
                packet.data().len()
            );

            muxer.push(packet).unwrap();
        }
    });

    let mut stream = tokio_stream::wrappers::ReceiverStream::new(chan_recv).map(Ok::<_, io::Error>);
    socket.send_all(&mut stream).await?;
    socket.close().await?;

    demuxer_task.await?;

    Ok(())
}

#[cfg(not(feature = "ac-ffmpeg"))]
fn main() {
    println!("Enable the ac-ffmpeg feature to run this example")
}




#[cfg(test)]
mod test {
    use std::time::{Duration, Instant};

    use bytes::BytesMut;
    use mpeg2ts_reader::packet::Packet;
    use srt_tokio::SrtSocket;
    use tokio::{fs::File, io::AsyncReadExt, time::{sleep, sleep_until}};
    use futures::SinkExt;

    const MAX_PACKET_SIZE: usize = 1316;
    const PACKET_SIZE: usize = 188;
    
    #[derive(Default)]
    struct MpegTsPace {
        first_ts: Option<(u64, Instant)>,
    }

    impl MpegTsPace {
        pub fn check_delay(&mut self, buf: &[u8]) -> Option<Duration> {
            let mut delay = Duration::ZERO;

            for chunk in buf.chunks(PACKET_SIZE) {
                if chunk.len() < PACKET_SIZE {
                    break;
                }

                assert_eq!(Packet::SYNC_BYTE, chunk[0]);
                let packet = Packet::new(chunk);
                let pcr = packet.adaptation_field().map(|x|x.pcr());
                // println!("pcr: {pcr:?}");

                if let Some(Ok(clock)) = pcr {
                    let value: u64 = clock.into();
                    let milli = 1000 * value / 27_000_000;
                    // println!("clock: {clock:?}, value {value}, milli {milli}");

                    match self.first_ts {
                        Some(first_ts) => {
                            if milli > first_ts.0 {
                                let delta_milli = milli - first_ts.0;
                                let elapsed = first_ts.1.elapsed().as_millis() as u64;
                                if delta_milli > elapsed {
                                    let d = Duration::from_millis(delta_milli - elapsed);
                                    if delay < d {
                                        delay = d;
                                    }
                                }
                            }
                        },
                        None => {
                            self.first_ts = Some((milli, Instant::now()));
                        }
                    }
                }
            }

            if delay > Duration::ZERO {
                Some(delay)
            } else {
                None
            }
        }
    }
    
    #[tokio::test]
    async fn test() {
        // let filename = "/tmp/output.ts";
        let filename = "/tmp/srt.ts";

        // let target_addr: std::net::SocketAddr = "127.0.0.1:1234".parse().unwrap();
        // let mut socket: Option<tokio::net::UdpSocket> = Option::None;
        // {
            
        //     let socket0 = tokio::net::UdpSocket::bind("0.0.0.0:0").await.unwrap();
        //     socket = Some(socket0);

        //     println!("sending to [{target_addr}]");
        // }q

        let addr = ":1234";
        let mut socket: Option<SrtSocket> = Option::None;
        {
            println!("listen at [{addr}]");
            let socket0 = SrtSocket::builder()
            .latency(Duration::from_millis(1000))
            .listen_on(addr)
            .await
            .unwrap();
            socket = Some(socket0);
        }

        let (chan_send, mut chan_recv) = tokio::sync::mpsc::channel(1024);
    

        let read_task = tokio::spawn(async move {
            let mut file = File::open(filename).await.unwrap();
            let mut buf = BytesMut::new();
            
    
            let mut pace = MpegTsPace::default();
            // let mut first_ts = Option::<(u64, Instant)>::None;
    
            loop {
                
                while buf.len() >= PACKET_SIZE {
                    
                    let avalable_len = buf.len().min(MAX_PACKET_SIZE);
                    let avalable_data = &buf[..avalable_len];
                    let mut cnt = avalable_len - avalable_len % PACKET_SIZE;
                    if let Some(delay) = pace.check_delay(avalable_data) {
                        sleep(delay).await;
                    }

                    // let mut cnt = 0;
                    // for chunk in avalable_data.chunks(PACKET_SIZE)
                    // {
                    //     if chunk.len() < PACKET_SIZE {
                    //         break;
                    //     }
    
                    //     cnt += PACKET_SIZE;
    
                    //     assert_eq!(Packet::SYNC_BYTE, chunk[0]);
                    //     let packet = Packet::new(chunk);
                    //     let pcr = packet.adaptation_field().map(|x|x.pcr());
                    //     // println!("pcr: {pcr:?}");
    
                    //     if let Some(Ok(clock)) = pcr {
                    //         let value: u64 = clock.into();
                    //         let milli = 1000 * value / 27_000_000;
                    //         println!("clock: {clock:?}, value {value}, milli {milli}");
    
                    //         if first_ts.is_none() {
                    //             first_ts = Some((milli, Instant::now()));
                    //         }
    
                    //         let first_ts = first_ts.unwrap();
                    //         if milli > first_ts.0 {
                    //             let delta_milli = milli - first_ts.0;
                    //             let elapsed = first_ts.1.elapsed().as_millis() as u64;
                    //             if delta_milli > elapsed {
                    //                 let delay = Duration::from_millis(delta_milli - elapsed);
                    //                 sleep(delay).await;
                    //             }
                    //         }
                    //     }
                    // }
    
                    let packet_data = buf.split_to(cnt).freeze();
                    chan_send.send((Instant::now(), packet_data)).await.unwrap();
    
                    // if let Some(socket) = &mut socket {
                    //     let now = Instant::now() ;
                    //     let _r = socket.send((now, packet_data)).await.unwrap();
                    // }
    
                    // if let Some(socket) = &mut socket {
                    //     socket.send_to(&packet_data, target_addr).await.unwrap();
                    // }
                }
    
                let mut read_bytes = 0;
                while read_bytes < MAX_PACKET_SIZE {
                    let r = file.read_buf(&mut buf).await.unwrap();
                    if r == 0 {
                        break;
                    }
                    read_bytes += r;
                }
                
                if read_bytes == 0 {
                    println!("EOF");
                    break;
                }
            }
        });

        while let Some(item) = chan_recv.recv().await {
            if let Some(socket) = &mut socket {
                println!("send srt data len {cnt}", cnt=item.1.len());
                let _r = socket.send(item).await.unwrap();
            }

            // if let Some(socket) = &mut socket {
            //     println!("send udp data len {cnt}", cnt=item.1.len());
            //     socket.send_to(&item.1, target_addr).await.unwrap();
            // }
        }

        read_task.await.unwrap();
    }
}
