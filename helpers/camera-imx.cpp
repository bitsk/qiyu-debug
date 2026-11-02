// SPDX-License-Identifier: GPL-3.0-or-later
// Bounded IMX471 observer for an ALREADY CONFIGURED pair. The device executor
// verifies slot4=PHY1 and slot5=PHY0 before invoking it. Never changes the graph,
// exposure, GPIOs or the tracking service. Shares the capture ownership lock.
#include <qiyu/device/camera-preview.hpp>
#include <cerrno>
#include <atomic>
#include <csignal>
#include <cstdio>
#include <fstream>
#include <future>
#include <linux/videodev2.h>
#include <poll.h>
#include <stdexcept>
#include <string>
#include <sys/file.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <vector>

static volatile sig_atomic_t stopped;
static void stop(int) { stopped = 1; }
static void check(int rc, const char *operation) {
    if (rc < 0) throw std::runtime_error(std::string(operation)+": "+std::strerror(errno));
}
static int call(int fd, unsigned long op, void *arg) {
    int rc;
    do { rc = ioctl(fd, op, arg); } while (rc < 0 && errno == EINTR && !stopped);
    return rc;
}
static std::uint64_t now() {
    timespec ts{}; check(clock_gettime(CLOCK_MONOTONIC, &ts), "clock");
    return std::uint64_t(ts.tv_sec)*1000000000+ts.tv_nsec;
}
class Camera {
    int fd_ = -1;
    bool streaming_ = false;
    unsigned stride_ = 0;
    struct Mapping { void *data; std::size_t size; };
    std::vector<Mapping> maps_;
    a081::preview::Tap tap_;
    static v4l2_buffer buffer(v4l2_plane &plane, unsigned index = 0) {
        v4l2_buffer b{}; b.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        b.memory = V4L2_MEMORY_MMAP; b.index = index; b.length = 1; b.m.planes = &plane;
        return b;
    }
public:
    std::uint64_t frames = 0, invalid = 0;
    int fd() const { return fd_; }
    void open(const char *path) {
        std::string node(path);
        if (node.compare(0, 10, "/dev/video") || node.size() == 10 || node.find_first_not_of("0123456789", 10) != std::string::npos)
            throw std::runtime_error("Expected configured /dev/videoN node");
        fd_ = ::open(path, O_RDWR | O_NONBLOCK | O_CLOEXEC | O_NOFOLLOW);
        check(fd_, "open camera");
        struct stat st{}; check(fstat(fd_, &st), "fstat");
        if (!S_ISCHR(st.st_mode)) throw std::runtime_error("Not a camera character device");
        v4l2_capability cap{}; check(call(fd_, VIDIOC_QUERYCAP, &cap), "QUERYCAP");
        if (std::strncmp(reinterpret_cast<char *>(cap.driver), "qcom-camss", sizeof(cap.driver)))
            throw std::runtime_error("Expected CAMSS capture node");
        v4l2_format fmt{}; fmt.type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        check(call(fd_, VIDIOC_G_FMT, &fmt), "G_FMT");
        const auto &p = fmt.fmt.pix_mp;
        stride_ = p.plane_fmt[0].bytesperline;
        if (p.width != 1928 || p.height != 1088 || p.num_planes != 1 || p.field != V4L2_FIELD_NONE ||
            p.pixelformat != V4L2_PIX_FMT_SRGGB10P || stride_ < 2410 || p.plane_fmt[0].sizeimage < stride_*1088)
            throw std::runtime_error("Configure verified IMX471 1928x1088 pRAA first");
        v4l2_requestbuffers req{}; req.type = fmt.type; req.memory = V4L2_MEMORY_MMAP; req.count = 8;
        check(call(fd_, VIDIOC_REQBUFS, &req), "REQBUFS");
        if (req.count < 2 || req.count > 32) throw std::runtime_error("Invalid buffer count");
        for (unsigned i = 0; i < req.count; ++i) {
            v4l2_plane plane{}; auto b = buffer(plane, i);
            check(call(fd_, VIDIOC_QUERYBUF, &b), "QUERYBUF");
            if (b.length != 1 || plane.length < p.plane_fmt[0].sizeimage) throw std::runtime_error("Invalid plane");
            void *data = mmap(nullptr, plane.length, PROT_READ | PROT_WRITE, MAP_SHARED, fd_, plane.m.mem_offset);
            if (data == MAP_FAILED) check(-1, "mmap");
            maps_.push_back({data, plane.length});
            check(call(fd_, VIDIOC_QBUF, &b), "QBUF");
        }
    }
    void start() {
        auto type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
        check(call(fd_, VIDIOC_STREAMON, &type), "STREAMON"); streaming_ = true;
    }
    void frame(unsigned slot) {
        v4l2_plane plane{}; auto b = buffer(plane);
        if (call(fd_, VIDIOC_DQBUF, &b) < 0) { if (errno == EAGAIN) return; check(-1, "DQBUF"); }
        const auto stamp = now();
        if (b.index >= maps_.size()) throw std::runtime_error("Invalid buffer index");
        const auto &map = maps_[b.index];
        if (!(b.flags & V4L2_BUF_FLAG_ERROR) && b.length == 1 &&
            plane.data_offset <= plane.bytesused && plane.bytesused <= map.size &&
            plane.bytesused-plane.data_offset >= std::size_t(stride_)*1088) {
            ++frames;
            tap_.rggb10(slot, b.sequence, stamp, static_cast<const std::uint8_t *>(map.data)+plane.data_offset,
                        plane.bytesused-plane.data_offset, stride_);
        } else { ++invalid; }
        // Nonblocking tap copies/sends before ownership goes back to DMA.
        v4l2_plane queued{}; queued.length = map.size; auto q = buffer(queued, b.index);
        check(call(fd_, VIDIOC_QBUF, &q), "requeue");
    }
    ~Camera() {
        if (streaming_) {
            auto type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
            if (call(fd_, VIDIOC_STREAMOFF, &type) < 0) std::perror("IMX STREAMOFF");
        }
        for (const auto &map : maps_) munmap(map.data, map.size);
        if (fd_ >= 0) close(fd_);
    }
};
int main(int argc, char **argv) {
    if (argc != 4) { std::fprintf(stderr, "Usage: camera-imx SLOT4_VIDEO SLOT5_VIDEO SECONDS(1..300)\n"); return 2; }
    int lock = -1, result = 0;
    try {
        std::ifstream file("/proc/device-tree/model", std::ios::binary);
        const std::string model((std::istreambuf_iterator<char>(file)), {});
        if (getuid() || model.find("A081") == std::string::npos || !std::getenv("A081_CAMERA_PREVIEW_SOCKET"))
            throw std::runtime_error("Requires root on A081 and explicit preview socket");
        char *end = nullptr; long seconds = std::strtol(argv[3], &end, 10);
        if (!*argv[3] || *end || seconds < 1 || seconds > 300 || std::strcmp(argv[1], argv[2]) == 0)
            throw std::runtime_error("Distinct slot nodes and bounded duration required");
        lock = ::open("/run/a081-camera.lock", O_CREAT | O_RDWR | O_CLOEXEC | O_NOFOLLOW, 0600);
        check(lock, "camera lock"); check(flock(lock, LOCK_EX | LOCK_NB), "Camera owned by tracking; stop via owner first");
        std::signal(SIGINT, stop); std::signal(SIGTERM, stop);
        Camera right, left; right.open(argv[1]); left.open(argv[2]);
        const auto deadline = now()+std::uint64_t(seconds)*1000000000;
        std::atomic<bool> failed{false};
        auto capture = [&](Camera &camera, unsigned slot) {
            try {
                camera.start();
                // Drain immediately; the other sensor's STREAMON may take
                // longer than the first sensor's entire buffer queue.
                while (!stopped && !failed.load() && now() < deadline) {
                    pollfd fd{camera.fd(), POLLIN, 0};
                    int rc = poll(&fd, 1, 100);
                    if (rc < 0 && errno == EINTR) continue;
                    check(rc, "poll");
                    if (fd.revents & (POLLERR | POLLHUP | POLLNVAL)) throw std::runtime_error("Camera poll error");
                    if (fd.revents & POLLIN) camera.frame(slot);
                }
            } catch (...) { failed.store(true); throw; }
        };
        auto first = std::async(std::launch::async, capture, std::ref(right), 4);
        try {
            auto second = std::async(std::launch::async, capture, std::ref(left), 5);
            first.get(); second.get();
        } catch (...) { failed.store(true); throw; }
        std::fprintf(stdout, "IMX slot4_frames=%llu slot4_invalid=%llu slot5_frames=%llu slot5_invalid=%llu\n",
            static_cast<unsigned long long>(right.frames), static_cast<unsigned long long>(right.invalid),
            static_cast<unsigned long long>(left.frames), static_cast<unsigned long long>(left.invalid));
        if (!right.frames || !left.frames) throw std::runtime_error("No complete IMX frames");
    } catch (const std::exception &e) { std::fprintf(stderr, "%s\n", e.what()); result = 1; }
    if (lock >= 0) close(lock);
    return result;
}
