// SPDX-License-Identifier: MIT
// Private compute context only. No surface, swapchain, modeset or DRM master.
#include <poll.h>
#include <vulkan/vulkan.h>
#include "shader.h"

struct gpu_times { uint64_t import_ns, dispatch_ns, readback_ns; };
static struct {
    VkInstance instance;
    VkPhysicalDevice physical;
    VkPhysicalDeviceMemoryProperties memory;
    VkDevice device;
    VkQueue queue;
    uint32_t family, max_storage_range, output_width, output_height;
    VkCommandPool pool;
    VkCommandBuffer command;
    VkDescriptorSetLayout descriptors;
    VkDescriptorPool descriptor_pool;
    VkDescriptorSet descriptor_set;
    VkPipelineLayout layout;
    VkPipeline pipeline;
    VkBuffer output;
    VkDeviceMemory output_memory;
    VkFence fence;
    void *mapped;
    size_t output_bytes;
    bool coherent, ready;
    dev_t primary;
    PFN_vkGetMemoryFdPropertiesKHR memory_fd_properties;
} gpu;

static void gpu_finish(void) {
    if (gpu.device) {
        vkDeviceWaitIdle(gpu.device);
        if (gpu.mapped) vkUnmapMemory(gpu.device,gpu.output_memory);
        if (gpu.fence) vkDestroyFence(gpu.device,gpu.fence,NULL);
        if (gpu.pipeline) vkDestroyPipeline(gpu.device,gpu.pipeline,NULL);
        if (gpu.layout) vkDestroyPipelineLayout(gpu.device,gpu.layout,NULL);
        if (gpu.descriptor_pool) vkDestroyDescriptorPool(gpu.device,gpu.descriptor_pool,NULL);
        if (gpu.descriptors) vkDestroyDescriptorSetLayout(gpu.device,gpu.descriptors,NULL);
        if (gpu.pool) vkDestroyCommandPool(gpu.device,gpu.pool,NULL);
        if (gpu.output) vkDestroyBuffer(gpu.device,gpu.output,NULL);
        if (gpu.output_memory) vkFreeMemory(gpu.device,gpu.output_memory,NULL);
        vkDestroyDevice(gpu.device,NULL);
    }
    if (gpu.instance) vkDestroyInstance(gpu.instance,NULL);
    memset(&gpu,0,sizeof(gpu));
}

static bool gpu_extension(VkPhysicalDevice physical,const char *name) {
    uint32_t count=0;
    if (vkEnumerateDeviceExtensionProperties(physical,NULL,&count,NULL)!=VK_SUCCESS) return false;
    VkExtensionProperties *items=calloc(count,sizeof(*items));
    if (!items) return false;
    bool found=false;
    if (vkEnumerateDeviceExtensionProperties(physical,NULL,&count,items)==VK_SUCCESS)
        for (uint32_t i=0;i<count;i++) if (!strcmp(items[i].extensionName,name)) found=true;
    free(items);return found;
}

static uint32_t gpu_memory_type(uint32_t bits,VkMemoryPropertyFlags required,VkMemoryPropertyFlags preferred) {
    uint32_t fallback=UINT32_MAX;
    for (uint32_t i=0;i<gpu.memory.memoryTypeCount;i++) {
        VkMemoryPropertyFlags flags=gpu.memory.memoryTypes[i].propertyFlags;
        if ((bits&(1U<<i)) && (flags&required)==required) {
            if ((flags&preferred)==preferred) return i;
            if (fallback==UINT32_MAX) fallback=i;
        }
    }
    return fallback;
}

#define GPU_CHECK(expression) do { VkResult r_=(expression); if(r_!=VK_SUCCESS) { \
    fprintf(stderr,"GPU failure line=%d VkResult=%d\n",__LINE__,r_); goto failed; } } while(0)

static bool gpu_init(dev_t primary,size_t output_bytes,uint32_t width,uint32_t height) {
    VkShaderModule shader=VK_NULL_HANDLE;
    // A081 deliberately separates the existing MSM render device from the
    // msm-kms primary. Match Vulkan to the validated render node, not card1.
    struct stat render_stat;
    if (lstat("/dev/dri/renderD128",&render_stat) || !S_ISCHR(render_stat.st_mode)) return false;
    int render_fd=open("/dev/dri/renderD128",O_RDONLY|O_CLOEXEC|O_NOFOLLOW);
    if (render_fd<0) return false;
    drmVersionPtr version=drmGetVersion(render_fd);
    bool render_valid=drmGetNodeTypeFromFd(render_fd)==DRM_NODE_RENDER &&
                      version && !strcmp(version->name,"msm");
    if (version) drmFreeVersion(version);
    close(render_fd);
    if (!render_valid) return false;
    VkApplicationInfo app={.sType=VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName="MIX read-only NV12 compute",.apiVersion=VK_API_VERSION_1_1};
    VkInstanceCreateInfo instance={.sType=VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,.pApplicationInfo=&app};
    GPU_CHECK(vkCreateInstance(&instance,NULL,&gpu.instance));
    uint32_t count=16;VkPhysicalDevice devices[16];
    GPU_CHECK(vkEnumeratePhysicalDevices(gpu.instance,&count,devices));
    for (uint32_t i=0;i<count;i++) {
        if (!gpu_extension(devices[i],VK_EXT_PHYSICAL_DEVICE_DRM_EXTENSION_NAME)) continue;
        VkPhysicalDeviceDrmPropertiesEXT drm={.sType=VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRM_PROPERTIES_EXT};
        VkPhysicalDeviceProperties2 props={.sType=VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,.pNext=&drm};
        vkGetPhysicalDeviceProperties2(devices[i],&props);
        if (drm.hasRender && drm.renderMajor==major(render_stat.st_rdev) &&
            drm.renderMinor==minor(render_stat.st_rdev) &&
            props.properties.apiVersion>=VK_API_VERSION_1_1) {
            gpu.physical=devices[i];
            gpu.max_storage_range=props.properties.limits.maxStorageBufferRange;
            fprintf(stderr,"GPU device=%s render=%u:%u kms_primary=%u:%u\n",props.properties.deviceName,
                    major(render_stat.st_rdev),minor(render_stat.st_rdev),major(primary),minor(primary));
            break;
        }
    }
    if (!gpu.physical) { fprintf(stderr,"No Vulkan physical device matches the validated MSM render node\n");goto failed; }
    const char *extensions[]={VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME,
        VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME,VK_EXT_QUEUE_FAMILY_FOREIGN_EXTENSION_NAME};
    for (unsigned i=0;i<sizeof(extensions)/sizeof(extensions[0]);i++)
        if (!gpu_extension(gpu.physical,extensions[i])) { fprintf(stderr,"Missing %s\n",extensions[i]);goto failed; }
    VkPhysicalDeviceExternalBufferInfo external={.sType=VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_BUFFER_INFO,
        .usage=VK_BUFFER_USAGE_STORAGE_BUFFER_BIT,.handleType=VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
    VkExternalBufferProperties external_props={.sType=VK_STRUCTURE_TYPE_EXTERNAL_BUFFER_PROPERTIES};
    vkGetPhysicalDeviceExternalBufferProperties(gpu.physical,&external,&external_props);
    if (!(external_props.externalMemoryProperties.externalMemoryFeatures&VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT) ||
        !(external_props.externalMemoryProperties.compatibleHandleTypes&VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT) ||
        output_bytes>gpu.max_storage_range)
        goto failed;
    uint32_t families=32;VkQueueFamilyProperties queue_properties[32];
    vkGetPhysicalDeviceQueueFamilyProperties(gpu.physical,&families,queue_properties);
    gpu.family=UINT32_MAX;
    for (uint32_t i=0;i<families;i++) if (queue_properties[i].queueCount &&
        (queue_properties[i].queueFlags&VK_QUEUE_COMPUTE_BIT)) {gpu.family=i;break;}
    if (gpu.family==UINT32_MAX) goto failed;
    float priority=0.25f;
    VkDeviceQueueCreateInfo queue={.sType=VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex=gpu.family,.queueCount=1,.pQueuePriorities=&priority};
    VkDeviceCreateInfo device={.sType=VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .queueCreateInfoCount=1,.pQueueCreateInfos=&queue,
        .enabledExtensionCount=sizeof(extensions)/sizeof(extensions[0]),.ppEnabledExtensionNames=extensions};
    GPU_CHECK(vkCreateDevice(gpu.physical,&device,NULL,&gpu.device));
    vkGetDeviceQueue(gpu.device,gpu.family,0,&gpu.queue);
    vkGetPhysicalDeviceMemoryProperties(gpu.physical,&gpu.memory);
    gpu.memory_fd_properties=(PFN_vkGetMemoryFdPropertiesKHR)vkGetDeviceProcAddr(gpu.device,"vkGetMemoryFdPropertiesKHR");
    if (!gpu.memory_fd_properties) goto failed;
    VkBufferCreateInfo output={.sType=VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,.size=output_bytes,
        .usage=VK_BUFFER_USAGE_STORAGE_BUFFER_BIT,.sharingMode=VK_SHARING_MODE_EXCLUSIVE};
    GPU_CHECK(vkCreateBuffer(gpu.device,&output,NULL,&gpu.output));
    VkMemoryRequirements requirements;vkGetBufferMemoryRequirements(gpu.device,gpu.output,&requirements);
    uint32_t type=gpu_memory_type(requirements.memoryTypeBits,VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT,
                                 VK_MEMORY_PROPERTY_HOST_CACHED_BIT);
    if (type==UINT32_MAX) goto failed;
    gpu.coherent=(gpu.memory.memoryTypes[type].propertyFlags&VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)!=0;
    VkMemoryAllocateInfo alloc={.sType=VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
        .allocationSize=requirements.size,.memoryTypeIndex=type};
    GPU_CHECK(vkAllocateMemory(gpu.device,&alloc,NULL,&gpu.output_memory));
    GPU_CHECK(vkBindBufferMemory(gpu.device,gpu.output,gpu.output_memory,0));
    GPU_CHECK(vkMapMemory(gpu.device,gpu.output_memory,0,VK_WHOLE_SIZE,0,&gpu.mapped));
    VkDescriptorSetLayoutBinding bindings[2]={
        {.binding=0,.descriptorType=VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,.descriptorCount=1,.stageFlags=VK_SHADER_STAGE_COMPUTE_BIT},
        {.binding=1,.descriptorType=VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,.descriptorCount=1,.stageFlags=VK_SHADER_STAGE_COMPUTE_BIT}};
    VkDescriptorSetLayoutCreateInfo layout={.sType=VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
        .bindingCount=2,.pBindings=bindings};
    GPU_CHECK(vkCreateDescriptorSetLayout(gpu.device,&layout,NULL,&gpu.descriptors));
    VkDescriptorPoolSize pool_size={.type=VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,.descriptorCount=2};
    VkDescriptorPoolCreateInfo pool={.sType=VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
        .maxSets=1,.poolSizeCount=1,.pPoolSizes=&pool_size};
    GPU_CHECK(vkCreateDescriptorPool(gpu.device,&pool,NULL,&gpu.descriptor_pool));
    VkDescriptorSetAllocateInfo set={.sType=VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
        .descriptorPool=gpu.descriptor_pool,.descriptorSetCount=1,.pSetLayouts=&gpu.descriptors};
    GPU_CHECK(vkAllocateDescriptorSets(gpu.device,&set,&gpu.descriptor_set));
    VkPushConstantRange push={.stageFlags=VK_SHADER_STAGE_COMPUTE_BIT,.offset=0,.size=24};
    VkPipelineLayoutCreateInfo pipeline_layout={.sType=VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
        .setLayoutCount=1,.pSetLayouts=&gpu.descriptors,.pushConstantRangeCount=1,.pPushConstantRanges=&push};
    GPU_CHECK(vkCreatePipelineLayout(gpu.device,&pipeline_layout,NULL,&gpu.layout));
    VkShaderModuleCreateInfo module={.sType=VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
        .codeSize=sizeof(nv12_shader),.pCode=nv12_shader};
    GPU_CHECK(vkCreateShaderModule(gpu.device,&module,NULL,&shader));
    const uint32_t dimensions[]={width,height};
    const VkSpecializationMapEntry dimension_entries[]={
        {.constantID=0,.offset=0,.size=sizeof(uint32_t)},
        {.constantID=1,.offset=sizeof(uint32_t),.size=sizeof(uint32_t)}};
    const VkSpecializationInfo specialization={
        .mapEntryCount=2,.pMapEntries=dimension_entries,
        .dataSize=sizeof(dimensions),.pData=dimensions};
    VkComputePipelineCreateInfo pipeline={.sType=VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
        .stage={.sType=VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
                .stage=VK_SHADER_STAGE_COMPUTE_BIT,.module=shader,.pName="main",
                .pSpecializationInfo=&specialization},.layout=gpu.layout};
    GPU_CHECK(vkCreateComputePipelines(gpu.device,VK_NULL_HANDLE,1,&pipeline,NULL,&gpu.pipeline));
    vkDestroyShaderModule(gpu.device,shader,NULL);shader=VK_NULL_HANDLE;
    VkCommandPoolCreateInfo command_pool={.sType=VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .flags=VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,.queueFamilyIndex=gpu.family};
    GPU_CHECK(vkCreateCommandPool(gpu.device,&command_pool,NULL,&gpu.pool));
    VkCommandBufferAllocateInfo command={.sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool=gpu.pool,.level=VK_COMMAND_BUFFER_LEVEL_PRIMARY,.commandBufferCount=1};
    GPU_CHECK(vkAllocateCommandBuffers(gpu.device,&command,&gpu.command));
    VkFenceCreateInfo fence={.sType=VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    GPU_CHECK(vkCreateFence(gpu.device,&fence,NULL,&gpu.fence));
    gpu.output_bytes=output_bytes;gpu.output_width=width;gpu.output_height=height;
    gpu.primary=primary;gpu.ready=true;
    fprintf(stderr,"GPU NV12 initialized output_bytes=%zu coherent=%d\n",output_bytes,gpu.coherent);
    return true;
failed:
    if (shader) vkDestroyShaderModule(gpu.device,shader,NULL);
    gpu_finish();return false;
}

static bool gpu_convert(int prime,dev_t primary,size_t dma_bytes,uint32_t pitch,uint32_t offset,
                        uint32_t width,uint32_t height,bool stereo,bool rgb10,
                        unsigned char *nv12,struct gpu_times *times) {
    size_t output_bytes=(size_t)width*height*3/2;
    if (width%4 || height%2 || pitch%4 || offset%4) return false;
    if (!gpu.ready && !gpu_init(primary,output_bytes,width,height)) return false;
    if (gpu.primary!=primary || gpu.output_bytes!=output_bytes ||
        gpu.output_width!=width || gpu.output_height!=height || dma_bytes>gpu.max_storage_range) return false;
    VkBuffer input=VK_NULL_HANDLE;VkDeviceMemory memory=VK_NULL_HANDLE;
    int imported=-1;bool submitted=false,success=false;
    uint64_t begin=ns();
    // Poll only published producer fences. Like the previous CPU capture this
    // does not freeze scanout/reuse: metadata must keep atomic_snapshot=false.
    struct pollfd poll_fd={.fd=prime,.events=POLLIN};
    int polled;do {polled=poll(&poll_fd,1,1000);} while(polled<0 && errno==EINTR && running);
    if (polled<=0 || !(poll_fd.revents&POLLIN)) goto failed;
    VkExternalMemoryBufferCreateInfo external={.sType=VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO,
        .handleTypes=VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
    VkBufferCreateInfo buffer={.sType=VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,.pNext=&external,
        .size=dma_bytes,.usage=VK_BUFFER_USAGE_STORAGE_BUFFER_BIT,.sharingMode=VK_SHARING_MODE_EXCLUSIVE};
    GPU_CHECK(vkCreateBuffer(gpu.device,&buffer,NULL,&input));
    VkMemoryRequirements requirements;vkGetBufferMemoryRequirements(gpu.device,input,&requirements);
    if (requirements.size>dma_bytes) goto failed;
    VkMemoryFdPropertiesKHR properties={.sType=VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR};
    GPU_CHECK(gpu.memory_fd_properties(gpu.device,VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,prime,&properties));
    uint32_t type=gpu_memory_type(requirements.memoryTypeBits&properties.memoryTypeBits,0,VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
    if (type==UINT32_MAX) goto failed;
    imported=fcntl(prime,F_DUPFD_CLOEXEC,0);if(imported<0) goto failed;
    VkMemoryDedicatedAllocateInfo dedicated={.sType=VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,.buffer=input};
    VkImportMemoryFdInfoKHR import={.sType=VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,.pNext=&dedicated,
        .handleType=VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,.fd=imported};
    VkMemoryAllocateInfo allocation={.sType=VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,.pNext=&import,
        .allocationSize=dma_bytes,.memoryTypeIndex=type};
    GPU_CHECK(vkAllocateMemory(gpu.device,&allocation,NULL,&memory));imported=-1;
    GPU_CHECK(vkBindBufferMemory(gpu.device,input,memory,0));
    VkDescriptorBufferInfo descriptors[2]={{.buffer=input,.range=dma_bytes},
                                          {.buffer=gpu.output,.range=output_bytes}};
    VkWriteDescriptorSet writes[2];memset(writes,0,sizeof(writes));
    for (uint32_t i=0;i<2;i++) writes[i]=(VkWriteDescriptorSet){.sType=VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
        .dstSet=gpu.descriptor_set,.dstBinding=i,.descriptorCount=1,
        .descriptorType=VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,.pBufferInfo=&descriptors[i]};
    vkUpdateDescriptorSets(gpu.device,2,writes,0,NULL);
    GPU_CHECK(vkResetCommandBuffer(gpu.command,0));
    VkCommandBufferBeginInfo command={.sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags=VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
    GPU_CHECK(vkBeginCommandBuffer(gpu.command,&command));
    VkBufferMemoryBarrier acquire={.sType=VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER,
        .srcAccessMask=0,.dstAccessMask=VK_ACCESS_SHADER_READ_BIT,
        .srcQueueFamilyIndex=VK_QUEUE_FAMILY_FOREIGN_EXT,.dstQueueFamilyIndex=gpu.family,
        .buffer=input,.offset=0,.size=VK_WHOLE_SIZE};
    vkCmdPipelineBarrier(gpu.command,VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                         0,0,NULL,1,&acquire,0,NULL);
    uint32_t params[]={width,height,pitch/4,offset/4,stereo,rgb10};
    vkCmdBindPipeline(gpu.command,VK_PIPELINE_BIND_POINT_COMPUTE,gpu.pipeline);
    vkCmdBindDescriptorSets(gpu.command,VK_PIPELINE_BIND_POINT_COMPUTE,gpu.layout,0,1,&gpu.descriptor_set,0,NULL);
    vkCmdPushConstants(gpu.command,gpu.layout,VK_SHADER_STAGE_COMPUTE_BIT,0,sizeof(params),params);
    vkCmdDispatch(gpu.command,(width/4+15)/16,(height/2+7)/8,1);
    VkBufferMemoryBarrier release[2]={
        {.sType=VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER,.srcAccessMask=VK_ACCESS_SHADER_READ_BIT,.dstAccessMask=0,
         .srcQueueFamilyIndex=gpu.family,.dstQueueFamilyIndex=VK_QUEUE_FAMILY_FOREIGN_EXT,
         .buffer=input,.offset=0,.size=VK_WHOLE_SIZE},
        {.sType=VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER,.srcAccessMask=VK_ACCESS_SHADER_WRITE_BIT,.dstAccessMask=VK_ACCESS_HOST_READ_BIT,
         .srcQueueFamilyIndex=VK_QUEUE_FAMILY_IGNORED,.dstQueueFamilyIndex=VK_QUEUE_FAMILY_IGNORED,
         .buffer=gpu.output,.offset=0,.size=VK_WHOLE_SIZE}};
    vkCmdPipelineBarrier(gpu.command,VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,VK_PIPELINE_STAGE_HOST_BIT,
                         0,0,NULL,2,release,0,NULL);
    GPU_CHECK(vkEndCommandBuffer(gpu.command));
    GPU_CHECK(vkResetFences(gpu.device,1,&gpu.fence));
    uint64_t ready=ns();times->import_ns=ready-begin;
    VkSubmitInfo submit={.sType=VK_STRUCTURE_TYPE_SUBMIT_INFO,.commandBufferCount=1,.pCommandBuffers=&gpu.command};
    GPU_CHECK(vkQueueSubmit(gpu.queue,1,&submit,gpu.fence));submitted=true;
    GPU_CHECK(vkWaitForFences(gpu.device,1,&gpu.fence,VK_TRUE,UINT64_MAX));submitted=false;
    uint64_t complete=ns();times->dispatch_ns=complete-ready;
    if (!gpu.coherent) {
        VkMappedMemoryRange range={.sType=VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE,
            .memory=gpu.output_memory,.offset=0,.size=VK_WHOLE_SIZE};
        GPU_CHECK(vkInvalidateMappedMemoryRanges(gpu.device,1,&range));
    }
    memcpy(nv12,gpu.mapped,output_bytes);
    times->readback_ns=ns()-complete;success=true;
failed:
    if (submitted) vkDeviceWaitIdle(gpu.device);
    if (imported>=0) close(imported);
    if (input) vkDestroyBuffer(gpu.device,input,NULL);
    if (memory) vkFreeMemory(gpu.device,memory,NULL);
    return success;
}
#undef GPU_CHECK
