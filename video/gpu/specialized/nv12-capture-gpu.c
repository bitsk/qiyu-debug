// SPDX-License-Identifier: MIT
// A081 read-only KMS live view, adapted from steamvr-scanout.py's verified
// GETFB2 -> PRIME export -> DMA-BUF CPU read sequence. Never modesets/takes master.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <linux/dma-buf.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <time.h>
#include <unistd.h>
#include <xf86drm.h>
#include <xf86drmMode.h>
#include <drm_fourcc.h>


static volatile sig_atomic_t running = 1;
static void stop(int unused) { (void)unused; running = 0; }
static uint64_t ns(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return (uint64_t)t.tv_sec*1000000000+t.tv_nsec; }
#include "gpu-nv12.h"

static bool read_text(const char *name, char *text, size_t length) {
    FILE *f=fopen(name,"r"); if (!f) return false;
    size_t n=fread(text,1,length-1,f); text[n]=0; bool ok=!ferror(f); fclose(f); return ok;
}
static int64_t number(const char *v, int low, int high) {
    char *end; long n=strtol(v,&end,10); if (*end || n<low || n>high) return -1; return n;
}
struct capture {
    unsigned char *argb, *nv12;
    unsigned width, height;
    uint32_t fb, crtc, fourcc;
    uint64_t sequence, end_sequence, vblank_ns, acquire_ns, convert_ns;
    bool sequence_valid, end_sequence_valid;
    struct gpu_times gpu_times;
    const char *mode;
};

static int frame(const char *card, struct capture *out) {
    const uint64_t began = ns();
    char name[256], state[32768], clients[8192], debug[128], path[192];
    struct stat st; if (lstat(card,&st) || !S_ISCHR(st.st_mode)) return 1;
    snprintf(debug,sizeof(debug),"/sys/kernel/debug/dri/%u",minor(st.st_rdev));
    snprintf(path,sizeof(path),"%s/name",debug);
    if (!read_text(path,name,sizeof(name)) || strncmp(name,"msm-kms ",8)) return 2;
    snprintf(path,sizeof(path),"%s/clients",debug);
    if (!read_text(path,clients,sizeof(clients))) return 3;
    // Do not open an unowned primary. A race with shutdown is rechecked below.
    bool master=false; char *line=clients;
    while (*line) {
        char comm[80], m=0; unsigned pid,dev;
        if (sscanf(line," %79s %u %u %c",comm,&pid,&dev,&m)==4 && m=='y') master=true;
        char *next=strchr(line,'\n'); if (!next) break; line=next+1;
    }
    if (!master) return 4;
    int fd=open(card,O_RDONLY|O_CLOEXEC|O_NOFOLLOW); if (fd<0) return 5;
    int result=6, prime=-1; drmModePlaneRes *resources=NULL; drmModeFB2 *fb=NULL;
    size_t bytes=0; uint32_t handles[4]={0};
    if (drmIsMaster(fd)!=0 || drmGetNodeTypeFromFd(fd)!=DRM_NODE_PRIMARY) goto done;
    if (drmSetClientCap(fd,DRM_CLIENT_CAP_UNIVERSAL_PLANES,1)<0) goto done;
    resources=drmModeGetPlaneResources(fd); if (!resources) goto done;
    // Read the actual scanout allocator from debugfs; never label a desktop as stereo.
    for (unsigned retry=0;retry<4 && !fb;retry++) {
        for (unsigned i=0;i<resources->count_planes && !fb;i++) {
            drmModePlane *p=drmModeGetPlane(fd,resources->planes[i]); if (!p) continue;
            uint32_t plane=p->plane_id, id=p->fb_id, crtc=p->crtc_id; bool active=crtc && id;
            drmModeFreePlane(p); if (!active) continue;
            snprintf(path,sizeof(path),"%s/state",debug);
            if (!read_text(path,state,sizeof(state))) continue;
            char prefix[48]; snprintf(prefix,sizeof(prefix),"plane[%u]:",plane);
            char *block=strstr(state,prefix); if (!block) continue;
            char *end=strstr(block+1,"\nplane["); if (!end) end=strstr(block+1,"\ncrtc[");
            if (end) *end=0;
            char expect[48]; snprintf(expect,sizeof(expect),"\tfb=%u\n",id);
            if (!strstr(block,expect)) continue;
            if (strstr(block,"allocated by = vrcompositor\n")) out->mode="stereo";
            else if (strstr(block,"allocated by = kwin_wayland\n")) out->mode="desktop";
            else continue;
            fb=drmModeGetFB2(fd,id);
            if (fb) out->crtc=crtc;
        }
    }
    result=7; if (!fb) goto done;
    out->sequence_valid=drmCrtcGetSequence(fd,out->crtc,&out->sequence,&out->vblank_ns)==0;
    out->fourcc=fb->pixel_format;
    for (int i=0;i<4;i++) handles[i]=fb->handles[i];
    result=8;
    bool rgb10=fb->pixel_format==DRM_FORMAT_XRGB2101010 || fb->pixel_format==DRM_FORMAT_ARGB2101010;
    if (fb->width!=1920 || fb->height!=3664 || fb->modifier!=DRM_FORMAT_MOD_LINEAR ||
        (fb->pixel_format!=DRM_FORMAT_XRGB8888 && fb->pixel_format!=DRM_FORMAT_ARGB8888 && !rgb10) ||
        !fb->handles[0] || fb->handles[1] || fb->handles[2] || fb->handles[3] ||
        fb->pitches[0]<7680 || fb->pitches[0]>65536 || fb->offsets[0]>16777216) goto done;
    bytes=(size_t)fb->offsets[0]+(size_t)fb->pitches[0]*fb->height;
    result=9; if (bytes>268435456 || drmPrimeHandleToFD(fd,fb->handles[0],O_CLOEXEC,&prime)<0) goto done;
    // Close fresh GETFB2 handles before CPU work; PRIME FD owns this read reference.
    for (int i=0;i<4;i++) if (handles[i]) {
        uint32_t h=handles[i];
        for (int j=i;j<4;j++) if (handles[j]==h) handles[j]=0;
        if (drmCloseBufferHandle(fd,h)<0) goto done;
    }
    result=10;
    off_t dma_bytes=lseek(prime,0,SEEK_END);
    if (dma_bytes<(off_t)bytes || dma_bytes>268435456) goto done;
    const uint64_t acquired=ns();
    out->acquire_ns=acquired-began;
    result=14;
    if (!gpu_convert(prime,st.st_rdev,(size_t)dma_bytes,fb->pitches[0],fb->offsets[0],
                     out->width,out->height,!strcmp(out->mode,"stereo"),rgb10,
                     out->nv12,&out->gpu_times)) goto done;
    uint64_t end_vblank_ns;
    out->end_sequence_valid=drmCrtcGetSequence(fd,out->crtc,&out->end_sequence,&end_vblank_ns)==0;
    out->convert_ns=ns()-acquired;
    out->fb=fb->fb_id; result=0;
done:
    if (prime>=0) close(prime);
    for (int i=0;i<4;i++) if (handles[i]) {
        uint32_t h=handles[i]; for (int j=i;j<4;j++) if (handles[j]==h) handles[j]=0;
        drmCloseBufferHandle(fd,h);
    }
    if (fb) drmModeFreeFB2(fb);
    if (resources) drmModeFreePlaneResources(resources);
    close(fd); return result;
}
int main(int argc,char **argv) {
    const char *card="/dev/dri/card1";int width=1280,fps=60;
    for (int i=1;i<argc;i+=2) {
        if (i+1>=argc) return 2;
        if (!strcmp(argv[i],"--card")) card=argv[i+1];
        else if (!strcmp(argv[i],"--width")) width=number(argv[i+1],640,1920);
        else if (!strcmp(argv[i],"--fps")) fps=number(argv[i+1],1,90);
        else return 2;
    }
    char identity[256];
    if (geteuid()!=0 || width<0 || width%4 || fps<0 || strncmp(card,"/dev/dri/card",13) ||
        !read_text("/etc/a081-rootfs-id",identity,sizeof(identity)) || strcmp(identity,"a081-steamos-arm64\n") ||
        !read_text("/proc/device-tree/model",identity,sizeof(identity)) || strcmp(identity,"iQIYI Qiyu MIX (A081)")) return 3;
    struct capture c={.width=(unsigned)width,.height=(((uint64_t)width*1920+3663)/3664+1)&~1U};
    size_t bytes=(size_t)c.width*c.height*3/2;
    c.nv12=malloc(bytes);
    unsigned char *previous=malloc(bytes); if (!c.nv12 || !previous) return 4;
    bool have_previous=false;
    uint64_t attempts=0, delivered=0, duplicates=0, missed_deadlines=0, last_sequence=0, sequence_skipped=0;
    uint32_t last_crtc=0;
    signal(SIGTERM,stop);signal(SIGINT,stop);signal(SIGPIPE,stop);
    setvbuf(stdout,NULL,_IONBF,0);
    while (running) {
        uint64_t begin=ns(); int result=frame(card,&c);
        ++attempts;
        uint64_t captured=ns();
        if (result) printf("{\"state\":\"waiting\",\"reason\":\"capture_unavailable\",\"capture_error\":%d,\"length\":0}\n",result);
        else {
            bool duplicate=have_previous && memcmp(previous,c.nv12,bytes)==0;
            bool new_sequence=c.sequence_valid && (!have_previous || c.crtc!=last_crtc || c.sequence>last_sequence);
            bool publish=!duplicate && new_sequence;
            if (duplicate) ++duplicates;
            if (!new_sequence) ++sequence_skipped;
            if (publish) { memcpy(previous,c.nv12,bytes); have_previous=true; last_sequence=c.sequence; last_crtc=c.crtc; ++delivered; }
            printf("{\"state\":\"live\",\"mode\":\"%s\",\"sequence\":%"PRIu64",\"attempts\":%"PRIu64",\"duplicate_content_skipped\":%"PRIu64",\"sequence_skipped\":%"PRIu64",\"missed_deadlines\":%"PRIu64",\"capture_begin_ns\":%"PRIu64",\"captured_ns\":%"PRIu64",\"capture_us\":%"PRIu64",\"acquire_us\":%"PRIu64",\"convert_us\":%"PRIu64",\"gpu_import_us\":%"PRIu64",\"gpu_dispatch_us\":%"PRIu64",\"gpu_readback_us\":%"PRIu64",\"width\":%u,\"height\":%u,\"source_fb\":%u,\"source_crtc\":%u,\"source_fourcc\":%u,\"vblank_sequence_valid\":%s,\"vblank_sequence\":%"PRIu64",\"vblank_end_sequence_valid\":%s,\"vblank_end_sequence\":%"PRIu64",\"vblank_ns\":%"PRIu64",\"conversion_backend\":\"vulkan-compute-dmabuf\",\"atomic_snapshot\":false,\"format\":\"NV12\",\"colorimetry\":\"bt601\",\"duplicate_content\":%s,\"length\":%zu}\n",
                c.mode,delivered,attempts,duplicates,sequence_skipped,missed_deadlines,begin,captured,(captured-begin)/1000,
                c.acquire_ns/1000,c.convert_ns/1000,c.gpu_times.import_ns/1000,c.gpu_times.dispatch_ns/1000,c.gpu_times.readback_ns/1000,c.width,c.height,c.fb,c.crtc,c.fourcc,
                c.sequence_valid?"true":"false",c.sequence,c.end_sequence_valid?"true":"false",c.end_sequence,c.vblank_ns,duplicate?"true":"false",publish?bytes:0);
            if (publish && fwrite(c.nv12,1,bytes,stdout)!=bytes) break;
        }
        uint64_t interval=result ? 1000000000ULL : 1000000000ULL/fps;
        uint64_t end=ns(); if (end-begin<interval) {
            uint64_t wait=interval-(end-begin); struct timespec t={wait/1000000000ULL,wait%1000000000ULL}; nanosleep(&t,NULL);
        } else if (!result) ++missed_deadlines;
    }
    gpu_finish();free(previous);free(c.nv12);return 0;
}
