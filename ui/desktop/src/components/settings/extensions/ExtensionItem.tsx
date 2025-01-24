import React from 'react';
import { FullExtensionConfig } from '../../../extensions';
import { Gear } from '../../icons';

type ExtensionItemProps = FullExtensionConfig & {
  onToggle: (id: string) => void;
  onConfigure: (extension: FullExtensionConfig) => void;
};

export const ExtensionItem: React.FC<ExtensionItemProps> = (props) => {
  const { id, name, description, enabled, onToggle, onConfigure } = props;

  return (
    <div className="bg-white dark:bg-gray-800 rounded-lg py-2 mb-2">
      <div className="flex justify-between items-center">
        <div className="">
          <div className="flex items-center">
            <h3 className="text-sm font-semibold text-textStandard">{name}</h3>
          </div>
          <p className="text-xs text-textSubtle mt-[2px]">{description}</p>
        </div>
        <div className="flex items-center gap-3">
          <button onClick={() => onConfigure(props)} className="">
            <Gear className="w-5 h-5 text-textSubtle hover:text-textStandard" />
          </button>
          <button
            onClick={() => onToggle(id)}
            className={`relative inline-flex h-6 w-11 items-center rounded-full ${
              enabled ? 'bg-indigo-500' : 'bg-gray-200 dark:bg-gray-600'
            } transition-colors duration-200 ease-in-out focus:outline-none`}
          >
            <span
              className={`inline-block h-5 w-5 transform rounded-full bg-white shadow ${
                enabled ? 'translate-x-[22px]' : 'translate-x-[2px]'
              } transition-transform duration-200 ease-in-out`}
            />
          </button>
        </div>
      </div>
    </div>
  );
};
