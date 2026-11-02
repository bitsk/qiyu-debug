// Read-only OpenVR background observer. Never submits frames or changes settings.
#include <openvr.h>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <csignal>
#include <cerrno>
#include <dlfcn.h>
#include <time.h>
#include <unistd.h>
#include <sys/stat.h>
static volatile sig_atomic_t running=1;
static void stop(int){running=0;}
static double now(){timespec t{};clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec/1e9;}
int main(int argc,char **argv){
 if(argc!=4||getuid()!=1000||!getenv("VR_PATHREG_OVERRIDE"))return 2;
 char comm_path[128];std::snprintf(comm_path,sizeof(comm_path),"/proc/%u/comm",unsigned(std::strtoul(argv[2],nullptr,10)));
 struct stat st{};if(stat(comm_path,&st)||st.st_uid!=1000)return 3;
 FILE *f=std::fopen(comm_path,"r");char comm[64]{};if(!f)return 3;
 bool existing=std::fgets(comm,sizeof(comm),f)&&!std::strcmp(comm,"vrcompositor\n");std::fclose(f);if(!existing)return 3;
 // A runtime must already exist; background init is never a boot mechanism.
 if(access(argv[3],R_OK))return 3;
 if(mkdir("/tmp/mix-openvr-observer",0700)&&errno!=EEXIST)return 8;
 struct stat logdir{};if(lstat("/tmp/mix-openvr-observer",&logdir)||!S_ISDIR(logdir.st_mode)||logdir.st_uid!=1000)return 8;
 void *lib=dlopen("/opt/steamvr/bin/linuxarm64/libopenvr_api.so",RTLD_NOW|RTLD_LOCAL);if(!lib)return 4;
 auto init=reinterpret_cast<uint32_t(*)(vr::EVRInitError*,vr::EVRApplicationType,const char*)>(dlsym(lib,"VR_InitInternal2"));
 auto get=reinterpret_cast<void*(*)(const char*,vr::EVRInitError*)>(dlsym(lib,"VR_GetGenericInterface"));
 auto shutdown=reinterpret_cast<void(*)()>(dlsym(lib,"VR_ShutdownInternal"));
 if(!init||!get||!shutdown)return 5;
 vr::EVRInitError error=vr::VRInitError_None;
 if(!init(&error,vr::VRApplication_Background,nullptr)||error!=vr::VRInitError_None)return 6;
 auto *compositor=static_cast<vr::IVRCompositor*>(get(vr::IVRCompositor_Version,&error));
 if(!compositor||error!=vr::VRInitError_None){shutdown();return 7;}
 signal(SIGTERM,stop);signal(SIGINT,stop);setvbuf(stdout,nullptr,_IOLBF,0);
 double end=now()+std::strtod(argv[1],nullptr);
 while(running&&now()<end){
  vr::Compositor_FrameTiming f{};f.m_nSize=sizeof(f);
  bool ok=compositor->GetFrameTiming(&f);
  vr::Compositor_CumulativeStats s{};compositor->GetCumulativeStats(&s,sizeof(s));
  std::printf("MIX_TIMING {\"monotonic_seconds\":%.9f,\"frame_valid\":%s,\"frame_index\":%u,\"frame_seconds\":%.9f,\"stats_pid\":%u,\"presents\":%u,\"submits\":%u,\"dropped\":%u,\"reprojected\":%u,\"compositor_gpu_ms\":%.6f,\"compositor_cpu_ms\":%.6f,\"total_render_gpu_ms\":%.6f,\"client_interval_ms\":%.6f,\"present_call_ms\":%.6f,\"wait_present_ms\":%.6f}\n",
   now(),ok?"true":"false",f.m_nFrameIndex,f.m_flSystemTimeInSeconds,s.m_nPid,s.m_nNumFramePresents,s.m_nNumFrameSubmits,s.m_nNumDroppedFrames,s.m_nNumReprojectedFrames,f.m_flCompositorRenderGpuMs,f.m_flCompositorRenderCpuMs,f.m_flTotalRenderGpuMs,f.m_flClientFrameIntervalMs,f.m_flPresentCallCpuMs,f.m_flWaitForPresentCpuMs);
  usleep(500000);
 }
 shutdown();dlclose(lib);return 0;
}
