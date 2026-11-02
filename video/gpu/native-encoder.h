// SPDX-License-Identifier: MIT
// Native NV12 DMA-BUF -> Venus. Python receives compressed AUs only.
#include <gst/gst.h>
#include <gst/app/gstappsrc.h>
#include <gst/app/gstappsink.h>
#include <gst/allocators/gstdmabuf.h>
#include <gst/video/video.h>
#include <pthread.h>
#include <stdatomic.h>

static struct {
    GstElement *pipeline, *source, *sink;
    GstAllocator *allocator;
    GstBus *bus;
    pthread_mutex_t lock;
    bool busy[4];
    atomic_bool failed;
    uint64_t origin, submitted, encoded;
    struct { uint64_t pts, input_ns; char meta[4096]; } pending[1024];
    const char *codec;
} enc={.lock=PTHREAD_MUTEX_INITIALIZER};

static void native_released(void *slot,GstMiniObject *object) {
    (void)object;
    pthread_mutex_lock(&enc.lock); enc.busy[(uintptr_t)slot]=false; pthread_mutex_unlock(&enc.lock);
}
static bool native_ok(void) {
    GstMessage *m=enc.bus ? gst_bus_pop_filtered(enc.bus,GST_MESSAGE_ERROR) : NULL;
    if (m) {
        GError *e=NULL;gchar *detail=NULL;gst_message_parse_error(m,&e,&detail);
        fprintf(stderr,"Native encoder: %s (%s)\n",e->message,detail?detail:"");
        g_error_free(e);g_free(detail);gst_message_unref(m);enc.failed=true;
    }
    return !enc.failed;
}
static GstPadProbeReturn native_input(GstPad *pad,GstPadProbeInfo *info,void *unused) {
    (void)pad;(void)unused;uint64_t now=ns();
    GstBuffer *buffer=GST_PAD_PROBE_INFO_BUFFER(info);
    if (!buffer) return GST_PAD_PROBE_OK;
    pthread_mutex_lock(&enc.lock);
    for (unsigned i=0;i<1024;i++) if (enc.pending[i].meta[0] && enc.pending[i].pts==GST_BUFFER_PTS(buffer)) {
        enc.pending[i].input_ns=now;break;
    }
    pthread_mutex_unlock(&enc.lock);return GST_PAD_PROBE_OK;
}
static GstFlowReturn native_sample(GstAppSink *sink,void *unused) {
    (void)unused;
    uint64_t output_ns=ns();
    GstSample *sample=gst_app_sink_pull_sample(sink);
    if (!sample) return GST_FLOW_EOS;
    GstBuffer *buffer=gst_sample_get_buffer(sample);
    GstMapInfo map;
    if (!gst_buffer_map(buffer,&map,GST_MAP_READ)) {gst_sample_unref(sample);return GST_FLOW_ERROR;}
    char meta[4096]={0};uint64_t input_ns=0;
    pthread_mutex_lock(&enc.lock);
    for (unsigned i=0;i<1024;i++) if (enc.pending[i].meta[0] && enc.pending[i].pts==GST_BUFFER_PTS(buffer)) {
        memcpy(meta,enc.pending[i].meta,sizeof(meta));input_ns=enc.pending[i].input_ns;
        enc.pending[i].meta[0]=0;break;
    }
    uint64_t encoded=++enc.encoded;
    pthread_mutex_unlock(&enc.lock);
    if (!meta[0]) {fprintf(stderr,"Unmatched native PTS %"PRIu64"\n",GST_BUFFER_PTS(buffer));enc.failed=true;}
    else {
        size_t length=strlen(meta);if (length && meta[length-1]=='}') meta[length-1]=0;
        printf("%s,\"native_encoder\":true,\"codec\":\"%s\",\"encoder_input_ns\":%"PRIu64",\"encoder_output_ns\":%"PRIu64",\"native_encoded\":%"PRIu64",\"input_memory\":\"vulkan-export-dmabuf-import\",\"nv12_stride\":%u,\"nv12_uv_offset\":%u,\"native_payload_length\":%zu}\n",
               meta,enc.codec,input_ns,output_ns,encoded,gpu_output_stride,gpu_uv_offset,map.size);
        if (fwrite(map.data,1,map.size,stdout)!=map.size) enc.failed=true;
    }
    gst_buffer_unmap(buffer,&map);gst_sample_unref(sample);
    return enc.failed ? GST_FLOW_ERROR : GST_FLOW_OK;
}
static bool native_init(const char *codec,unsigned width,unsigned height,unsigned fps,unsigned bitrate) {
    gst_init(NULL,NULL);enc.codec=codec;enc.origin=ns();
    // Venus driver pads NV12 pitch to 128 and luma scanlines to 32. VideoMeta
    // advertises the actual layout; dmabuf-import must accept it, never copy.
    gpu_output_stride=(width+127)&~127U;
    gpu_uv_offset=gpu_output_stride*((height+31)&~31U);
    gpu_output_size=(gpu_uv_offset+gpu_output_stride*(((height+1)/2+15)&~15U)+8191)&~4095U;
    gpu_export_output=true;
    gchar *factory=NULL;GList *features=gst_registry_get_feature_list(gst_registry_get(),GST_TYPE_ELEMENT_FACTORY);
    for (GList *l=features;l;l=l->next) {
        const char *name=gst_plugin_feature_get_name(l->data);
        char suffix[32];snprintf(suffix,sizeof(suffix),"%senc",codec);
        if (!g_str_has_prefix(name,"v4l2") || !g_str_has_suffix(name,suffix)) continue;
        GstElement *element=gst_element_factory_create(l->data,NULL);gchar *device=NULL;
        g_object_get(element,"device",&device,NULL);
        char sys[256],identity[128];
        if (device) {
            snprintf(sys,sizeof(sys),"/sys/class/video4linux/%s/name",strrchr(device,'/')+1);
            if (read_text(sys,identity,sizeof(identity)) && !strcmp(identity,"qcom-venus-encoder\n")) {
                if (factory) {g_free(device);gst_object_unref(element);gst_plugin_feature_list_free(features);g_free(factory);return false;}
                factory=g_strdup(name);fprintf(stderr,"Native Venus factory=%s device=%s\n",name,device);
            }
        }
        g_free(device);gst_object_unref(element);
    }
    gst_plugin_feature_list_free(features);if(!factory) return false;
    const bool h265=!strcmp(codec,"h265");
    const bool high=width*height>2073600 || width>1920 || bitrate>50000000;
    gchar *pipeline=g_strdup_printf(
        "appsrc name=source is-live=true format=time block=false max-buffers=4 max-bytes=0 "
        "! %s name=encoder output-io-mode=dmabuf-import capture-io-mode=mmap "
        "! video/x-%s,profile=%s,level=(string)%s ! %sparse config-interval=-1 "
        "! video/x-%s,stream-format=byte-stream,alignment=au "
        "! appsink name=sink sync=false async=false emit-signals=true max-buffers=4 drop=false",
        factory,codec,h265?"main":"constrained-baseline",h265||high?"5.1":"4.2",codec,codec);
    GError *error=NULL;enc.pipeline=gst_parse_launch(pipeline,&error);g_free(pipeline);g_free(factory);
    if (error) {fprintf(stderr,"Native pipeline: %s\n",error->message);g_error_free(error);return false;}
    enc.source=gst_bin_get_by_name(GST_BIN(enc.pipeline),"source");enc.sink=gst_bin_get_by_name(GST_BIN(enc.pipeline),"sink");
    GstCaps *caps=gst_caps_new_simple("video/x-raw","format",G_TYPE_STRING,"NV12","width",G_TYPE_INT,width,
        "height",G_TYPE_INT,height,"framerate",GST_TYPE_FRACTION,fps,1,"colorimetry",G_TYPE_STRING,"bt601",
        "interlace-mode",G_TYPE_STRING,"progressive","pixel-aspect-ratio",GST_TYPE_FRACTION,1,1,NULL);
    gst_app_src_set_caps(GST_APP_SRC(enc.source),caps);gst_caps_unref(caps);
    GstElement *encoder=gst_bin_get_by_name(GST_BIN(enc.pipeline),"encoder");
    GstStructure *controls=gst_structure_new("controls","video_bitrate",G_TYPE_INT,bitrate,
        "video_gop_size",G_TYPE_INT,fps,h265?"hevc_profile":"h264_profile",G_TYPE_INT,h265?0:1,
        h265?"hevc_level":"h264_level",G_TYPE_INT,h265?8:high?15:13,NULL);
    g_object_set(encoder,"extra-controls",controls,NULL);gst_structure_free(controls);
    GstPad *input=gst_element_get_static_pad(encoder,"sink");
    gst_pad_add_probe(input,GST_PAD_PROBE_TYPE_BUFFER,native_input,NULL,NULL);gst_object_unref(input);gst_object_unref(encoder);
    g_signal_connect(enc.sink,"new-sample",G_CALLBACK(native_sample),NULL);
    enc.allocator=gst_dmabuf_allocator_new();enc.bus=gst_element_get_bus(enc.pipeline);
    return gst_element_set_state(enc.pipeline,GST_STATE_PLAYING)!=GST_STATE_CHANGE_FAILURE;
}
static int native_slot(int previous) {
    int selected=-1;pthread_mutex_lock(&enc.lock);
    for (int i=0;i<4;i++) if (!enc.busy[i] && i!=previous) {selected=i;break;}
    pthread_mutex_unlock(&enc.lock);return selected;
}
static bool native_push(int slot,const char *meta,uint64_t captured,unsigned width,unsigned height) {
    int fd=fcntl(gpu.export_fd,F_DUPFD_CLOEXEC,0);if(fd<0)return false;
    GstMemory *memory=gst_dmabuf_allocator_alloc(enc.allocator,fd,gpu.output_bytes);
    if (!memory) {close(fd);return false;}
    GstBuffer *buffer=gst_buffer_new();gst_buffer_append_memory(buffer,memory);
    gsize offsets[GST_VIDEO_MAX_PLANES]={0,gpu_uv_offset};gint strides[GST_VIDEO_MAX_PLANES]={(gint)gpu_output_stride,(gint)gpu_output_stride};
    gst_buffer_add_video_meta_full(buffer,GST_VIDEO_FRAME_FLAG_NONE,GST_VIDEO_FORMAT_NV12,width,height,2,offsets,strides);
    GST_BUFFER_PTS(buffer)=captured-enc.origin;GST_BUFFER_DTS(buffer)=GST_CLOCK_TIME_NONE;
    GST_BUFFER_DURATION(buffer)=GST_CLOCK_TIME_NONE;
    pthread_mutex_lock(&enc.lock);
    unsigned i=enc.submitted%1024;
    if (enc.pending[i].meta[0]) {pthread_mutex_unlock(&enc.lock);gst_buffer_unref(buffer);return false;}
    enc.pending[i].pts=GST_BUFFER_PTS(buffer);enc.pending[i].input_ns=0;
    g_strlcpy(enc.pending[i].meta,meta,sizeof(enc.pending[i].meta));enc.submitted++;enc.busy[slot]=true;
    pthread_mutex_unlock(&enc.lock);
    gst_mini_object_weak_ref(GST_MINI_OBJECT(buffer),native_released,(void *)(uintptr_t)slot);
    return gst_app_src_push_buffer(GST_APP_SRC(enc.source),buffer)==GST_FLOW_OK;
}
static void native_finish(void) {
    if (enc.pipeline) gst_element_set_state(enc.pipeline,GST_STATE_NULL);
    if (enc.source) gst_object_unref(enc.source);
    if (enc.sink) gst_object_unref(enc.sink);
    if (enc.bus) gst_object_unref(enc.bus);
    if (enc.pipeline) gst_object_unref(enc.pipeline);
    if (enc.allocator) gst_object_unref(enc.allocator);
}
