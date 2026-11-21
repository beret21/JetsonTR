# JetsonTR

[English](README.md) | **한국어**

**JetsonTR**은 Thermalright Trofeo Vision 9.16" USB LCD(1920×480)를 **NVIDIA Jetson AGX Xavier**의 시스템 모니터로 바꿔 줍니다.
USB bulk 전송으로 화면을 직접 보내므로 제조사 드라이버가 필요 없습니다.

macOS 버전인 [MacTR](https://github.com/beret21/MacTR)의 Jetson판입니다. TR은 Thermalright를 뜻합니다.

![미리보기](img/preview.png)

## 하드웨어

- **디스플레이:** Thermalright Trofeo Vision 9.16 LCD (9.16인치, 1920×480 IPS, USB Type-C, 자석 거치)
  - 제조사: [블랙](https://www.thermalright.com/product/trofeo-vision-9-16-lcd-black/) · [화이트](https://www.thermalright.com/product/trofeo-vision-9-16-lcd-white/)
  - 아마존: [Trofeo Vision 9.16 LCD Black](https://www.amazon.com/dp/B0FX7SH64X)
  - 테스트한 장치: ARGB가 없는 모델, USB VID `0x0416` / PID `0x5408`. ARGB 모델은 테스트하지 않았습니다.
- **호스트:** NVIDIA Jetson AGX Xavier 16GB (JetPack 5.1.6 / L4T R35.6)

## 화면 구성

| 패널 | 내용 | 출처 |
|---|---|---|
| CPU | Carmel 8코어(2코어 × 4클러스터)의 코어별 사용률과 주파수, 온도, 부하, 프로세스 수 | `/proc/stat`, `cpufreq`, `thermal_zone`, `/proc/loadavg` |
| GPU | Volta GPU 사용률과 주파수, EMC(메모리 대역폭) 사용률, 온도, NVENC/NVDEC/VIC 동작 여부, 네트워크 추이 | `17000000.gv11b/load`, `devfreq`, `actmon_avg_activity/mc_all` + EMC 클럭(root), `host1x/*/load`, `/proc/net/dev` |
| MEMORY | 통합 메모리의 Used / Cache / zram / Available, zram 스왑 입출력 추이 | `/proc/meminfo`, `/sys/block/zram*/mm_stat`, `/proc/vmstat` |
| POWER | 전체 전력과 레일별 전력(W), 온도 센서 6곳, 팬 rpm과 PWM | INA3221 `hwmon`(root 필요), `thermal_zone`, `pwm_tach` |
| SYSTEM | 가동 시간, JetsonTR 자체의 CPU·메모리 사용량, NVMe 사용량, 디스크 I/O 추이 | `/proc/uptime`, `/proc/self`, `statvfs`, `/proc/diskstats` |

## 요구 사항

- Jetson AGX Xavier, JetPack 5.x(L4T R35)에서 테스트했습니다. 다른 Jetson 모델은 sysfs 경로가 다를 수 있습니다.
- 빌드에 Rust 툴체인(stable), `libusb-1.0` 개발 파일, `cmake`와 C 컴파일러가 필요합니다. libjpeg-turbo를 소스에서 빌드해 정적으로 링크합니다.
- 글꼴: DejaVu Sans (`fonts-dejavu-core`), 실행할 때 읽어 들입니다.

```bash
sudo apt install libusb-1.0-0-dev pkg-config cmake build-essential fonts-dejavu-core
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # Rust가 없을 때
```

## 빌드

```bash
cargo build --release
sudo install -m 755 target/release/jetsontr /usr/local/bin/jetsontr
```

결과물은 약 1MB짜리 실행 파일 하나이며, libusb 말고는 실행에 필요한 것이 없습니다.

## 사용법

```bash
# 장치 없이 한 프레임을 PNG로 렌더링
jetsontr --png img/preview.png

# 패널 정보 조회 / 10초 컬러 테스트 화면
jetsontr --handshake-only
jetsontr --test 10

# LCD 구동 (전력 레일 값은 root에서만 읽힘)
sudo jetsontr --rotate 0
```

| 옵션 | 설명 |
|---|---|
| `--rotate DEG` | `0` 또는 `180` (기본값 `180`). 장치에서 화면이 바로 보이는 쪽을 고르세요 |
| `--no-rotate` | `--rotate 0`과 같음 |
| `--brightness F` | 보내기 전에 RGB에 곱하는 밝기 배율, 0.1~5.0 (기본값 1.3) |
| `--fast SECS` | CPU, GPU, EMC, 전력, 메모리, 네트워크, 디스크 I/O 갱신 주기 (기본값 5) |
| `--slow SECS` | 온도, 팬, zram, 프로세스 수 갱신 주기 (기본값 15) |
| `--fixed SECS` | 전원 모드(nvpmodel), 디스크 사용량 갱신 주기 (기본값 60) |
| `--png PATH` | 장치 없이 PNG 한 장을 저장하고 종료 |
| `--handshake-only` | 패널을 조회해 PM/SUB/FBL 출력 |
| `--test SECS` | 컬러 테스트 화면 표시 |

모든 설정은 명령행 인수로 받고, 주지 않은 값은 기본값을 씁니다. 화면에 시계가 없으므로 빠름 주기로 값을 갱신할 때만 다시 그립니다. 상태 표시는 몇 초 간격의 대략적인 값이면 충분하고, 주기를 늘린 만큼 부하가 줄어듭니다.
서비스 설정을 바꾸려면 `sudo systemctl edit --full thermalright-lcd`로 `ExecStart`를 고치고 저장하세요.

### 서비스로 상시 실행 (systemd)

```bash
# 일반 사용자 접근 허용 (plugdev 그룹)
sudo cp udev/99-thermalright.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger

# 서비스 설치 (전력 레일을 읽기 위해 root로 실행)
sudo cp systemd/thermalright-lcd.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now thermalright-lcd
journalctl -u thermalright-lcd -f
```

## 동작 방식

- **스레드:** 수집 스레드는 빠름/느림/고정 세 단계 주기로 값을 갱신하고, 빠름 주기마다 새 프레임을 그려 JPEG로 압축합니다. 프레임이 끊기면 LCD가 꺼지기 때문에 메인 루프는 캐시된 JPEG를 0.5초마다 다시 보냅니다.
- **프로토콜 (LY)**
  - 핸드셰이크: 2048B를 보내고 512B 응답을 받아 PM/SUB/FBL을 얻습니다. 이 장치는 PM=65, FBL=192, 1920×480으로 응답합니다.
  - 프레임: JPEG(650KB 이하)를 16B 헤더와 최대 496B 데이터로 된 512B 패킷으로 나눕니다. 패킷 수는 크기/496을 올림한 값이고 4의 배수로 채웁니다. 패킷은 최대 64KB씩(USB 패킷 분할은 libusb가 처리) bulk OUT(0x09)으로 보내고, bulk IN(0x81)에서 512B ACK를 읽습니다.
  - 패널은 4:2:0 크로마 서브샘플링의 baseline JPEG만 해독합니다. 4:4:4 JPEG를 보내면 화면이 비어 보입니다.
  - 전원을 켠 뒤 첫 핸드셰이크는 `resp[8]=0x02`(준비 안 됨)를 돌려줄 수 있어서 재시도합니다.
- **복구:** 전송이 실패하거나 장치가 분리되면 장치를 닫고 5초 뒤 다시 엽니다. `nvpmodel`은 3초 타임아웃을 걸어 호출합니다.
- **코드 구성:** `src/lcd.rs`(USB 프로토콜), `src/metrics.rs`(수집), `src/render.rs`(tiny-skia·ab_glyph로 그리기, libjpeg-turbo로 JPEG 인코딩), `src/main.rs`(스레드와 명령행).

### 성능 (Jetson AGX Xavier, MAXN)

| | Python 시제품 | Rust, 1초 갱신 | Rust, 현재 |
|---|---|---|---|
| 프레임 (그리기 + 변환 + JPEG) | 약 290ms + JPEG | 약 92ms | 약 55ms |
| 화면 갱신 | 2초 | 1초 | 5초 |
| CPU (서비스 + tegrastats, 코어 1개 기준) | 약 14% | 약 5.3% | 약 0.9% |
| 메모리 (RSS) | 약 72MB | 약 11MB | 약 16MB |

줄인 방법: 시계를 없애 5초 갱신으로 충분하게 했고, GPU·EMC·전력은 `tegrastats` 자식 프로세스 대신 sysfs에서 직접 읽습니다. 변하지 않는 배경은 한 번만 그려 재사용하고, JPEG은 순수 Rust 인코더 대신 libjpeg-turbo(NEON)로 압축합니다. 밝기는 조회 테이블로 적용하고, USB는 4KB 대신 64KB 단위로 씁니다. SYSTEM 패널에 JetsonTR 자신의 CPU(8코어 전체 대비)와 메모리 사용량을 표시해 비용을 바로 확인할 수 있습니다.

## 감사의 말

- [thermalright-trcc-linux](https://github.com/Lexonight1/thermalright-trcc-linux): LY bulk 프로토콜 역공학 
- [MacTR](https://github.com/beret21/MacTR): 이 화면 레이아웃의 바탕이 된 macOS 버전
- [Claude Code](https://claude.com/claude-code)로 만들었습니다. Mac 세션과 Jetson 세션이 세션 간 메시지로 프로토콜 문서와 화면 사양을 주고받았습니다.

## 라이선스

MIT. [LICENSE](LICENSE)를 참고하세요.
LY 프로토콜 정보(USB ID, 핸드셰이크, 패킷 형식)는 [thermalright-trcc-linux](https://github.com/Lexonight1/thermalright-trcc-linux)(GPL-3.0)를 참조했습니다. 구현은 직접 했습니다. Rust로 작성했고 구조도 다르며, 그 프로젝트의 코드나 C#에서 넘어온 관행은 가져오지 않았습니다. 이 저장소에는 그 프로젝트의 코드가 포함되어 있지 않습니다.
