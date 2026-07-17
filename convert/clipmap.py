import numpy as np
import pyktx
import argparse
from PIL import Image
Image.MAX_IMAGE_PIXELS = 933120000


if __name__ == "__main__":
    def cmd_ktx(args):
        print(f"Convert {args.filename} to KTX2 {args.width}x{args.height}")

        heightmap = np.array(Image.open(args.filename).resize(
            (args.width, args.height)), dtype=np.float32).astype(np.uint16)

        texture = pyktx.KtxTexture2.create(pyktx.KtxTextureCreateInfo(
            gl_internal_format=None,
            base_width=heightmap.shape[1],
            base_height=heightmap.shape[0],
            base_depth=1,
            num_dimensions=2,
            num_levels=1,
            num_layers=1,
            num_faces=1,
            is_array=False,
            vk_format=pyktx.VkFormat.VK_FORMAT_R16_UNORM,
            generate_mipmaps=False,
        ), pyktx.KtxTextureCreateStorage.ALLOC)
        texture.set_image_from_memory(
            level=0,
            layer=0,
            face_slice=0,
            data=heightmap.tobytes('C'),
        )
        ktx_filename = '.'.join(args.filename.split('.')[:-1])
        ktx_filename += f'_{args.width}x{args.height}.ktx2'
        texture.write_to_named_file(ktx_filename)
        print('Done.')

    parser = argparse.ArgumentParser(
        description='Heightmap processing tool for the bevy_wilderness plugin')
    parser.add_argument('filename', help='16-bit PNG heightmap')

    subparsers = parser.add_subparsers(dest='command', required=True)

    p_ktx = subparsers.add_parser('ktx', help='Convert the heightmap to KTX2')
    p_ktx.add_argument('width', type=int, help='Output width')
    p_ktx.add_argument('height', type=int, help='Output height')
    p_ktx.set_defaults(func=cmd_ktx)

    args = parser.parse_args()
    args.func(args)
