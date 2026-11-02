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
#include <turbojpeg.h>

static volatile sig_atomic_t running = 1;
static void stop(int unused) { (void)unused; running = 0; }
static uint64_t ns(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return (uint64_t)t.tv_sec*1000000000+t.tv_nsec; }
static bool read_text(const char *name, char *text, size_t length) {
    FILE *f=fopen(name,"r"); if (!f) return false;
    size_t n=fread(text,1,length-1,f); text[n]=0; bool ok=!ferror(f); fclose(f); return ok;
}
static int64_t number(const char *v, int low, int high) {
    char *end; long n=strtol(v,&end,10); if (*end || n<low || n>high) return -1; return n;
}
struct capture { unsigned char *rgb; unsigned width,height; uint32_t fb; const char *mode; };

static int frame(const char *card, struct capture *out) {
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
    void *mapped=MAP_FAILED; size_t bytes=0; uint32_t handles[4]={0};
    if (drmIsMaster(fd)!=0 || drmGetNodeTypeFromFd(fd)!=DRM_NODE_PRIMARY) goto done;
    if (drmSetClientCap(fd,DRM_CLIENT_CAP_UNIVERSAL_PLANES,1)<0) goto done;
    resources=drmModeGetPlaneResources(fd); if (!resources) goto done;
    // Read the actual scanout allocator from debugfs; never label a desktop as stereo.
    for (unsigned retry=0;retry<4 && !fb;retry++) {
        for (unsigned i=0;i<resources->count_planes && !fb;i++) {
            drmModePlane *p=drmModeGetPlane(fd,resources->planes[i]); if (!p) continue;
            uint32_t plane=p->plane_id, id=p->fb_id; bool active=p->crtc_id && id;
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
        }
    }
    result=7; if (!fb) goto done;
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
    result=10; if (lseek(prime,0,SEEK_END)<(off_t)bytes) goto done;
    mapped=mmap(NULL,bytes,PROT_READ,MAP_SHARED,prime,0); if (mapped==MAP_FAILED) goto done;
    struct dma_buf_sync sync={.flags=DMA_BUF_SYNC_START|DMA_BUF_SYNC_READ};
    result=11; if (ioctl(prime,DMA_BUF_IOCTL_SYNC,&sync)<0) goto done;
    unsigned half=out->width/2;
    bool stereo=strcmp(out->mode,"stereo")==0;
    unsigned char *pixels=(unsigned char *)mapped+fb->offsets[0];
    // Existing HMD ComputeDistortion rotates each eye clockwise onto the panel.
    // Inverse here is CCW: source x=W-1-output_y, source y=output_x.
    for (unsigned y=0;y<out->height;y++) {
        unsigned sx=1919-(uint64_t)y*1920/out->height;
        for (unsigned x=0;x<out->width;x++) {
            unsigned sy=stereo ? (x/half)*1832+(uint64_t)(x%half)*1832/half : (uint64_t)x*3664/out->width;
            const unsigned char *p=pixels+(size_t)sy*fb->pitches[0]+sx*4;
            unsigned char *q=out->rgb+((size_t)y*out->width+x)*3;
            if (rgb10) {
                // Little-endian AR30/XR30: R[29:20], G[19:10], B[9:0].
                // Normalize each full 10-bit component to 8-bit with rounding.
                uint32_t pixel; memcpy(&pixel,p,sizeof(pixel));
                q[0]=(((pixel>>20)&1023)*255+511)/1023;
                q[1]=(((pixel>>10)&1023)*255+511)/1023;
                q[2]=((pixel&1023)*255+511)/1023;
            } else {
                q[0]=p[2];q[1]=p[1];q[2]=p[0];
            }
        }
    }
    sync.flags=DMA_BUF_SYNC_END|DMA_BUF_SYNC_READ;
    result=12; if (ioctl(prime,DMA_BUF_IOCTL_SYNC,&sync)<0) goto done;
    out->fb=fb->fb_id; result=0;
done:
    if (mapped!=MAP_FAILED) munmap(mapped,bytes);
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
    const char *card="/dev/dri/card1";int width=1280,fps=5,quality=75;
    for (int i=1;i<argc;i+=2) {
        if (i+1>=argc) return 2;
        if (!strcmp(argv[i],"--card")) card=argv[i+1];
        else if (!strcmp(argv[i],"--width")) width=number(argv[i+1],640,1920);
        else if (!strcmp(argv[i],"--fps")) fps=number(argv[i+1],1,15);
        else if (!strcmp(argv[i],"--quality")) quality=number(argv[i+1],40,90);
        else return 2;
    }
    char identity[256];
    if (geteuid()!=0 || width<0 || width%2 || fps<0 || quality<0 || strncmp(card,"/dev/dri/card",13) ||
        !read_text("/etc/a081-rootfs-id",identity,sizeof(identity)) || strcmp(identity,"a081-steamos-arm64\n") ||
        !read_text("/proc/device-tree/model",identity,sizeof(identity)) || strcmp(identity,"iQIYI Qiyu MIX (A081)")) return 3;
    struct capture c={.width=(unsigned)width,.height=(((uint64_t)width*1920+3663)/3664+1)&~1U};
    c.rgb=malloc((size_t)c.width*c.height*3); if (!c.rgb) return 4;
    tjhandle compressor=tjInitCompress(); if (!compressor) return 5;
    unsigned char *jpeg=NULL; unsigned long jpeg_size=0; uint64_t seq=0;
    signal(SIGTERM,stop);signal(SIGINT,stop);signal(SIGPIPE,stop);
    setvbuf(stdout,NULL,_IONBF,0);
    while (running) {
        uint64_t begin=ns(); int result=frame(card,&c);
        if (!result) {
            int rc=tjCompress2(compressor,c.rgb,c.width,0,c.height,TJPF_RGB,&jpeg,&jpeg_size,TJSAMP_420,quality,TJFLAG_FASTDCT);
            if (rc) result=13;
        }
        uint64_t captured=ns();
        if (result) printf("{\"state\":\"waiting\",\"reason\":\"capture_unavailable\",\"capture_error\":%d,\"length\":0}\n",result);
        else {
            printf("{\"state\":\"live\",\"mode\":\"%s\",\"sequence\":%"PRIu64",\"captured_ns\":%"PRIu64",\"capture_us\":%"PRIu64",\"width\":%u,\"height\":%u,\"source_fb\":%u,\"atomic_snapshot\":false,\"length\":%lu}\n",c.mode,++seq,captured,(captured-begin)/1000,c.width,c.height,c.fb,jpeg_size);
            if (fwrite(jpeg,1,jpeg_size,stdout)!=jpeg_size) break;
        }
        uint64_t interval=result ? 1000000000ULL : 1000000000ULL/fps;
        uint64_t end=ns(); if (end-begin<interval) {
            uint64_t wait=interval-(end-begin); struct timespec t={wait/1000000000ULL,wait%1000000000ULL}; nanosleep(&t,NULL);
        }
    }
    if (jpeg) tjFree(jpeg);
    tjDestroy(compressor);free(c.rgb);return 0;
}
